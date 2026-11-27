//! Search over the number of planets K.
//!
//! Two pieces share the birth machinery:
//! - `Search`, the live map. It keeps the current planets, the accumulated paths and the pending
//!   birth work (candidate planets and partial joint refits). A new path gets a cheap local refit
//!   (`Search::add_path`); `Search::step` does a bounded slice of birth work and can be called at
//!   any time. A path that arrives between slices does not restart the pending work. Both work on
//!   the thinned rows (`rows.rs`) under the noise frozen for the operation (`Slice::noise`).
//! - `grow` and `prune`, the greedy birth and death descents of the from-scratch cascade
//!   (`rebuild.rs`).
//!
//! Birth: seed one extra planet where the observed windows' unexplained mean acceleration is
//! most significant (the residual points toward the missing mass, whose distance follows from
//! `|a| = mass / d^2` for a few radii; several diverse seeds, no grid). Each seed is first fitted
//! alone against the field the current planets leave over (cheap), then the best few are refitted
//! jointly with every planet. Death: drop each of the weakest planets, refit the rest jointly and
//! compare evidence. Contacts never create planets: they enter the joint fits only as gated
//! surface constraints.
use super::Shot;
use super::fit::{Budget, Evidence, Fit, any_overlap, overlapping, planet_at, theta_of};
use super::noise::{Noise, apply};
use super::rows::thin_rows;
use super::work::{Part, PartCeilings, Stats, par_map};
use crate::protocol::Planet;
use crate::simulation::WorldParameters;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Windows less significant than this (whitened magnitude; about 1.25 on average for a correct
/// model) never seed a planet.
const SEED_SIGNIFICANCE: f64 = 4.0;
/// Seeds are at least this far apart (world units).
const SEED_SEPARATION: f64 = 100.0;
const MAX_SEEDS: usize = 8;
const SEED_RADII: [f64; 3] = [20.0, 30.0, 40.0];
/// Seeds fitted alone that go on to a joint refit.
const JOINT_REFITS: usize = 3;
/// Weakest planets tried as deaths per level.
const DEATH_TRIALS: usize = 2;
/// Levels of the death descent of the cascade (planets removed one by one from the best mode).
pub(super) const DEATH_DEPTH: usize = 6;
/// Modes closer than this in centre and radius are the same planet.
const SAME_PLANET: (f64, f64) = (10.0, 3.0);

/// Rows kept by the live search (see `rows.rs`).
const ROW_CAP: usize = 1000;
/// A new path's local refit frees the planets within this distance of its rows.
const LOCAL_RADIUS: f64 = 100.0;
/// Evaluations of a local refit.
const LOCAL_EVALUATIONS: u64 = 300;
/// Alone fits per batch (a batch is the unit between wall-clock checks).
const BATCH: usize = 8;
/// A joint partial that is still improving after this many slices of work counts as finished.
const MAX_ROUNDS: usize = 6;
/// Candidates closer than this (xy) are one hypothesis; the list is bounded.
const DUPLICATE: f64 = 30.0;
const MAX_CANDIDATES: usize = 24;

#[derive(Clone, Debug)]
pub(super) struct Mode {
    pub planets: Vec<Planet>,
    /// Approximate log evidence including the planet prior; the flat model's is `-0.5 * chi2`.
    pub log_evidence: f64,
}

/// Limits of a from-scratch run (the cascade).
pub(super) struct Limits<'a> {
    pub max_planets: usize,
    /// Deterministic bound on residual plus Jacobian evaluations. It is checked between search
    /// stages and shared out equally among the refits of a stage, so the result does not depend
    /// on timing or thread scheduling; a stage may overshoot it by the evidence evaluations.
    pub max_evaluations: Option<u64>,
    /// Threads of the run.
    pub threads: usize,
    pub ceilings: &'a PartCeilings,
}

/// The limits and the work done so far, for sizing the budget of each stage.
pub(super) struct Run<'a> {
    pub limits: &'a Limits<'a>,
    pub stats: &'a Stats,
    pub cancelled: &'a AtomicBool,
}

impl Run<'_> {
    pub(super) fn work_exhausted(&self) -> bool {
        self.limits
            .max_evaluations
            .is_some_and(|max| self.stats.evaluations() >= max)
    }

    /// Budget for each of `tasks` parallel refits of the phase `part`: an equal share of the
    /// evaluations left, and the rest of the part's wall-clock ceiling (so a runaway phase is cut
    /// and reported instead of running on).
    fn budget(&self, tasks: usize, part: Part) -> Budget {
        let left = self
            .limits
            .max_evaluations
            .map(|max| max.saturating_sub(self.stats.evaluations()));
        Budget {
            threads: (self.limits.threads / tasks.max(1)).max(1),
            evaluations: left.map_or(u64::MAX, |left| left.div_ceil(tasks.max(1) as u64)),
            deadline: self
                .limits
                .ceilings
                .0
                .get(&part)
                .map(|ceiling| Instant::now() + ceiling.saturating_sub(self.stats.total(part))),
        }
    }
}

fn same_planet(a: &Planet, b: &Planet) -> bool {
    (a.x - b.x).hypot(a.y - b.y) < SAME_PLANET.0 && (a.radius - b.radius).abs() < SAME_PLANET.1
}

fn distance(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

pub(super) fn mode_at(joint: &Fit, theta: &[[f64; 3]]) -> Option<Mode> {
    let Evidence { log_evidence, .. } = joint.evidence(theta, 1)?;
    Some(Mode {
        planets: theta.iter().map(|th| planet_at(*th)).collect(),
        log_evidence,
    })
}

/// The refit of `start` as a mode; `None` when it cannot be evaluated or two of its planets
/// overlap (`fit::overlapping`: the generator never places such maps).
pub(super) fn refit(joint: &Fit, start: &[[f64; 3]], budget: &Budget) -> Option<Mode> {
    let (theta, _) = joint.refine(start, budget)?;
    if any_overlap(&theta) {
        return None;
    }
    mode_at(joint, &theta)
}

fn overlaps_any(planet: &[f64; 3], theta: &[[f64; 3]]) -> bool {
    theta.iter().any(|other| overlapping(planet, other))
}

/// Seed planets from the largest unexplained windows of the paths under `theta`; a seed that
/// overlaps a planet of `theta` is dropped.
fn seeds(joint: &Fit, theta: &[[f64; 3]], world: WorldParameters) -> Vec<[f64; 3]> {
    let mut chosen: Vec<[f64; 2]> = Vec::new();
    let mut seeds = Vec::new();
    for window in joint.unexplained(theta) {
        let magnitude = window.acceleration[0].hypot(window.acceleration[1]);
        if window.significance < SEED_SIGNIFICANCE
            || chosen.len() >= MAX_SEEDS
            || magnitude <= 0.0
            || chosen.iter().any(|p| {
                (p[0] - window.position[0]).hypot(p[1] - window.position[1]) < SEED_SEPARATION
            })
        {
            continue;
        }
        chosen.push(window.position);
        for radius in SEED_RADII {
            let distance = (0.1 * radius.powi(3) / magnitude).sqrt();
            let towards = |axis: usize| {
                window.position[axis] + window.acceleration[axis] / magnitude * distance
            };
            let seed = [
                towards(0).clamp(radius, world.battlefield_width - radius),
                towards(1).clamp(radius, world.battlefield_height - radius),
                radius,
            ];
            if !overlaps_any(&seed, theta) {
                seeds.push(seed);
            }
        }
    }
    seeds
}

/// Joint refits of `parent` plus one more planet, best evidence first.
fn births(
    shots: &[Shot],
    joint: &Fit,
    parent: &Mode,
    world: WorldParameters,
    run: &Run,
) -> Vec<Mode> {
    let stats = run.stats;
    let threads = run.limits.threads;
    let theta: Vec<[f64; 3]> = parent.planets.iter().map(theta_of).collect();
    let seeds = stats.time(Part::Seeding, || seeds(joint, &theta, world));
    if seeds.is_empty() {
        return Vec::new();
    }
    // Stage 1: the new planet alone against the field the parent leaves over.
    let alone =
        Fit::new(shots, world, run.cancelled, false, parent.planets.clone()).with_stats(stats);
    let budget = run.budget(seeds.len(), Part::BirthAlone);
    let mut fitted: Vec<([f64; 3], f64)> = stats
        .time(Part::BirthAlone, || {
            par_map(&seeds, threads, |seed| alone.refine(&[*seed], &budget))
        })
        .into_iter()
        .flatten()
        .map(|(theta, chi2)| (theta[0], chi2))
        .collect();
    fitted.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut distinct: Vec<[f64; 3]> = Vec::new();
    for (planet, _) in fitted {
        if distinct.len() < JOINT_REFITS
            && !distinct
                .iter()
                .any(|other| same_planet(&planet_at(*other), &planet_at(planet)))
        {
            distinct.push(planet);
        }
    }
    // Stage 2: every planet on every path.
    let budget = run.budget(distinct.len(), Part::BirthJoint);
    let mut modes: Vec<Mode> = stats
        .time(Part::BirthJoint, || {
            par_map(&distinct, threads, |planet| {
                let mut start = theta.clone();
                start.push(*planet);
                refit(joint, &start, &budget)
            })
        })
        .into_iter()
        .flatten()
        .collect();
    modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
    modes
}

/// Joint refits of `parent` without each of its weakest planets, best evidence first.
fn deaths(joint: &Fit, parent: &Mode, run: &Run) -> Vec<Mode> {
    let theta: Vec<[f64; 3]> = parent.planets.iter().map(theta_of).collect();
    let Some(full) = joint.chi2(&theta) else {
        return Vec::new();
    };
    let without = |i: usize| {
        let mut rest = theta.clone();
        rest.remove(i);
        rest
    };
    let mut weakest: Vec<(usize, f64)> = (0..theta.len())
        .filter_map(|i| Some((i, joint.chi2(&without(i))? - full)))
        .collect();
    weakest.sort_by(|a, b| a.1.total_cmp(&b.1));
    weakest.truncate(DEATH_TRIALS);
    let budget = run.budget(weakest.len(), Part::Deaths);
    let mut modes: Vec<Mode> = run
        .stats
        .time(Part::Deaths, || {
            par_map(&weakest, run.limits.threads, |(i, _)| {
                refit(joint, &without(*i), &budget)
            })
        })
        .into_iter()
        .flatten()
        .collect();
    modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
    modes
}

pub(super) fn best(modes: &[Mode]) -> &Mode {
    modes
        .iter()
        .max_by(|a, b| a.log_evidence.total_cmp(&b.log_evidence))
        .expect("the flat mode is always present")
}

/// Greedy births from the best mode until no birth improves the evidence, the planet cap or the
/// work limit is reached.
pub(super) fn grow(
    joint: &Fit,
    shots: &[Shot],
    modes: &mut Vec<Mode>,
    run: &Run,
    world: WorldParameters,
) {
    loop {
        let parent = best(modes).clone();
        if parent.planets.len() >= run.limits.max_planets
            || run.work_exhausted()
            || run.cancelled.load(Ordering::Relaxed)
        {
            break;
        }
        let children = births(shots, joint, &parent, world, run);
        let improved = children
            .first()
            .is_some_and(|child| child.log_evidence > parent.log_evidence);
        modes.extend(children);
        if !improved {
            break;
        }
    }
}

/// Death descent from the best mode: always continuing from the best child even when it is worse,
/// because planets that cancel each other (spurious leftovers of earlier, thinner data) only look
/// useful one at a time.
pub(super) fn prune(joint: &Fit, modes: &mut Vec<Mode>, run: &Run, depth: usize) {
    let mut parent = best(modes).clone();
    for _ in 0..depth {
        if parent.planets.is_empty()
            || run.work_exhausted()
            || run.cancelled.load(Ordering::Relaxed)
        {
            break;
        }
        let children = deaths(joint, &parent, run);
        let Some(next) = children.first().cloned() else {
            break;
        };
        modes.extend(children);
        parent = next;
    }
}

struct Candidate {
    theta: [f64; 3],
    /// Chi2 of the map plus this planet when last scored; `None`: never fitted.
    chi2: Option<f64>,
}

struct Partial {
    /// The map plus the candidate planet (last), or the map minus a weak planet (deaths), refit
    /// jointly.
    theta: Vec<[f64; 3]>,
    done: bool,
    rounds: usize,
}

/// What a slice or a local refit may use.
pub(super) struct Slice<'a> {
    /// Evaluations (checked between batches, so a slice may exceed it by one batch).
    pub evaluations: u64,
    pub deadline: Instant,
    pub threads: usize,
    pub max_planets: usize,
    pub cancelled: &'a AtomicBool,
    /// Set when new work (a path) is waiting: the slice stops as at its deadline and keeps its
    /// state for the next one. Nothing sets it in deterministic replays.
    pub preempt: &'a AtomicBool,
    pub stats: &'a Stats,
    /// The noise of every fit of the slice.
    pub noise: &'a Noise,
}

impl Slice<'_> {
    /// Whether the slice must stop now (deadline passed or pre-empted), keeping its best points.
    fn cut(&self) -> bool {
        Instant::now() >= self.deadline || self.preempt.load(Ordering::Relaxed)
    }
}

/// The live map and its pending birth work.
#[derive(Default)]
pub(super) struct Search {
    /// Every path so far (full rows, floor noise).
    paths: Vec<Shot>,
    /// The current best-estimate planets.
    pub theta: Vec<[f64; 3]>,
    /// Birth (or death) work is pending.
    pending: bool,
    seeded: bool,
    candidates: Vec<Candidate>,
    partials: Vec<Partial>,
    /// A death trial (the two planets whose removal costs least chi2, refit jointly, kept when
    /// the evidence rises) follows every birth.
    deaths_due: bool,
    /// At the planet cap a death trial has run since the last path or adoption (a successful one
    /// frees a slot, so the next birth makes it a swap).
    cap_trial_done: bool,
}

impl Search {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn idle(&self) -> bool {
        !self.pending
    }

    /// The thinned rows of every path under `noise`.
    fn thin(&self, noise: &Noise) -> Vec<Shot> {
        let mut shots = thin_rows(&self.paths, ROW_CAP);
        apply(&mut shots, noise);
        shots
    }

    fn clear_pending(&mut self) {
        self.candidates.clear();
        self.partials.clear();
        self.seeded = false;
        self.deaths_due = false;
    }

    fn finish(&mut self) {
        self.pending = false;
        self.clear_pending();
    }

    /// Replace the map by an audit's (refit on the paths of a few seconds ago): pending work is
    /// dropped and the search starts over.
    pub(super) fn adopt(&mut self, theta: Vec<[f64; 3]>) {
        self.theta = theta;
        self.clear_pending();
        self.pending = true;
        self.cap_trial_done = false;
    }

    /// A new path: refit the planets within `LOCAL_RADIUS` of its rows (the others are fixed
    /// field) on the thinned rows, and mark the pending work stale: candidates keep their alone
    /// fits (re-scored when the next round ranks them) and the joint partials resume from where
    /// they were.
    pub(super) fn add_path(&mut self, shot: &Shot, slice: &Slice) {
        self.paths.push(shot.clone());
        let near = |t: &[f64; 3]| {
            shot.path.as_ref().is_some_and(|p| {
                (0..p.row_count()).any(|r| {
                    let c = p.center(r);
                    (t[0] - c[0]).hypot(t[1] - c[1]) < LOCAL_RADIUS
                })
            })
        };
        let (free, fixed): (Vec<[f64; 3]>, Vec<[f64; 3]>) =
            self.theta.iter().partition(|t| near(t));
        if !free.is_empty() && !slice.cancelled.load(Ordering::Relaxed) {
            let shots = self.thin(slice.noise);
            let local = Fit::new(
                &shots,
                WorldParameters::default(),
                slice.cancelled,
                false,
                fixed.iter().map(|t| planet_at(*t)).collect(),
            )
            .with_stats(slice.stats);
            let budget = Budget {
                evaluations: LOCAL_EVALUATIONS,
                deadline: None,
                threads: slice.threads,
            };
            let refitted = slice
                .stats
                .time(Part::WarmRefit, || local.refine(&free, &budget));
            // `refine` keeps the free planets off each other and off the fixed ones.
            self.theta = fixed;
            self.theta.extend(refitted.map_or(free, |(t, _)| t));
        }
        self.pending = true;
        self.seeded = false;
        self.cap_trial_done = false;
        for partial in &mut self.partials {
            partial.done = false;
            partial.rounds = 0;
        }
    }

    /// One slice of birth work, until `slice.evaluations` are spent (checked between batches) or
    /// `slice.deadline`. Returns whether the map changed.
    pub(super) fn step(&mut self, slice: &Slice) -> bool {
        let world = WorldParameters::default();
        let (stats, cancelled) = (slice.stats, slice.cancelled);
        let first = stats.evaluations();
        let mut changed = false;
        // The paths and the noise do not change within a slice.
        let shots = self.thin(slice.noise);
        while !self.idle()
            && stats.evaluations() - first < slice.evaluations
            && !slice.cut()
            && !cancelled.load(Ordering::Relaxed)
        {
            if self.theta.len() >= slice.max_planets && !self.deaths_due && self.partials.is_empty()
            {
                // At the cap no birth is attempted, but a map holding a spurious planet must not
                // freeze there: one death trial per path, whose success frees a slot.
                if self.cap_trial_done {
                    self.pending = false;
                    break;
                }
                self.cap_trial_done = true;
                self.deaths_due = true;
                continue;
            }
            let joint = Fit::new(&shots, world, cancelled, true, vec![])
                .with_cut(slice.deadline, slice.preempt)
                .with_stats(stats);
            let left = slice.evaluations - (stats.evaluations() - first);
            if self.deaths_due && self.partials.is_empty() {
                let chi2 = joint.chi2(&self.theta);
                let mut weakest: Vec<(usize, f64)> = (0..self.theta.len())
                    .filter_map(|i| {
                        let mut rest = self.theta.clone();
                        rest.remove(i);
                        Some((i, joint.chi2(&rest)? - chi2?))
                    })
                    .collect();
                weakest.sort_by(|a, b| a.1.total_cmp(&b.1));
                self.partials = weakest
                    .iter()
                    .take(DEATH_TRIALS)
                    .map(|(i, _)| {
                        let mut theta = self.theta.clone();
                        theta.remove(*i);
                        Partial {
                            theta,
                            done: false,
                            rounds: 0,
                        }
                    })
                    .collect();
                if self.partials.is_empty() {
                    self.deaths_due = false;
                }
                continue;
            }
            if !self.seeded && self.partials.is_empty() {
                let new = stats.time(Part::Seeding, || seeds(&joint, &self.theta, world));
                for seed in new {
                    // A seed near a kept candidate is the same hypothesis.
                    if !self
                        .candidates
                        .iter()
                        .any(|c| distance(&c.theta, &seed) < DUPLICATE)
                    {
                        self.candidates.push(Candidate {
                            theta: seed,
                            chi2: None,
                        });
                    }
                }
                // Bounded list: the new seeds (unfitted) first, then the fitted candidates by their
                // last chi2.
                self.candidates.sort_by(|a, b| {
                    a.chi2
                        .unwrap_or(f64::NEG_INFINITY)
                        .total_cmp(&b.chi2.unwrap_or(f64::NEG_INFINITY))
                });
                self.candidates.truncate(MAX_CANDIDATES);
                self.seeded = true;
                if self.candidates.is_empty() {
                    self.finish();
                }
                continue;
            }
            let alone = || {
                Fit::new(
                    &shots,
                    world,
                    cancelled,
                    false,
                    self.theta.iter().map(|t| planet_at(*t)).collect(),
                )
                .with_cut(slice.deadline, slice.preempt)
                .with_stats(stats)
            };
            // Candidates not fitted on the current rows and map: alone against the field of the map.
            let todo: Vec<usize> = (0..self.candidates.len())
                .filter(|&i| self.partials.is_empty() && self.candidates[i].chi2.is_none())
                .take(BATCH)
                .collect();
            if !todo.is_empty() {
                let budget = Budget {
                    evaluations: (left / todo.len() as u64).clamp(15, 150),
                    deadline: Some(slice.deadline),
                    threads: (slice.threads / todo.len()).max(1),
                };
                let alone = alone();
                let fitted = stats.time(Part::BirthAlone, || {
                    par_map(&todo, slice.threads, |&i| {
                        alone.refine(&[self.candidates[i].theta], &budget)
                    })
                });
                // An alone fit cut by the deadline or a pre-emption has not settled: it keeps its
                // point and resumes from there in the next slice.
                let settled = !slice.cut();
                for (&i, fit) in todo.iter().zip(fitted) {
                    self.candidates[i] = match fit {
                        Some((t, chi2)) => Candidate {
                            theta: t[0],
                            chi2: settled.then_some(chi2),
                        },
                        None => Candidate {
                            theta: self.candidates[i].theta,
                            chi2: Some(f64::INFINITY),
                        },
                    };
                }
                continue;
            }
            if self.partials.is_empty() {
                // Re-score every fitted candidate against the current rows and map.
                let alone = alone();
                let rescored = par_map(&self.candidates, slice.threads, |c| alone.chi2(&[c.theta]));
                for (c, chi2) in self.candidates.iter_mut().zip(rescored) {
                    // A candidate overlapping a planet born since it was fitted is out.
                    c.chi2 = Some(
                        chi2.filter(|_| !overlaps_any(&c.theta, &self.theta))
                            .unwrap_or(f64::INFINITY),
                    );
                }
                let mut ranked: Vec<&Candidate> = self
                    .candidates
                    .iter()
                    .filter(|c| c.chi2.is_some_and(f64::is_finite))
                    .collect();
                ranked.sort_by(|a, b| a.chi2.unwrap().total_cmp(&b.chi2.unwrap()));
                let mut distinct: Vec<[f64; 3]> = Vec::new();
                for c in ranked {
                    if distinct.len() < JOINT_REFITS
                        && !distinct.iter().any(|d| {
                            distance(d, &c.theta) < SAME_PLANET.0
                                && (d[2] - c.theta[2]).abs() < SAME_PLANET.1
                        })
                    {
                        distinct.push(c.theta);
                    }
                }
                self.partials = distinct
                    .into_iter()
                    .map(|c| {
                        let mut theta = self.theta.clone();
                        theta.push(c);
                        Partial {
                            theta,
                            done: false,
                            rounds: 0,
                        }
                    })
                    .collect();
                if self.partials.is_empty() {
                    self.finish();
                    continue;
                }
            }
            // Joint refits, resumed from where the last slice left them.
            let todo: Vec<usize> = (0..self.partials.len())
                .filter(|&i| !self.partials[i].done)
                .collect();
            if !todo.is_empty() {
                let part = if self.deaths_due {
                    Part::Deaths
                } else {
                    Part::BirthJoint
                };
                let each = (left / todo.len() as u64).max(30);
                let budget = Budget {
                    evaluations: each,
                    deadline: Some(slice.deadline),
                    threads: (slice.threads / todo.len()).max(1),
                };
                let fits = stats.time(part, || {
                    par_map(&todo, slice.threads, |&i| {
                        let counter = Stats::default();
                        let out = Fit::new(&shots, world, cancelled, true, vec![])
                            .with_cut(slice.deadline, slice.preempt)
                            .with_stats(&counter)
                            .refine(&self.partials[i].theta, &budget);
                        (out, counter)
                    })
                });
                for (&i, (out, counter)) in todo.iter().zip(fits) {
                    let p = &mut self.partials[i];
                    p.rounds += 1;
                    if let Some((t, _)) = out {
                        p.theta = t;
                    }
                    // A refit cut by the slice deadline or a pre-emption has not settled: it
                    // resumes in the next slice.
                    p.done =
                        (counter.evaluations() < each && !slice.cut()) || p.rounds >= MAX_ROUNDS;
                    stats.absorb(&counter);
                }
                continue;
            }
            // Every joint refit has settled: adopt the best if it beats the map's evidence.
            let parent = mode_at(&joint, &self.theta).map_or(f64::NEG_INFINITY, |m| m.log_evidence);
            let child = self
                .partials
                .iter()
                .filter(|p| !any_overlap(&p.theta))
                .filter_map(|p| mode_at(&joint, &p.theta).map(|m| (m.log_evidence, &p.theta)))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            let deaths = self.deaths_due;
            self.deaths_due = false;
            match child {
                Some((evidence, theta))
                    if evidence > parent && (deaths || theta.len() <= slice.max_planets) =>
                {
                    let theta = theta.clone();
                    self.deaths_due = !deaths;
                    if !deaths {
                        // The candidates stay as warm starts (re-scored next round); the one just
                        // born is spent.
                        let born = *theta.last().unwrap();
                        self.candidates.retain(|c| distance(&c.theta, &born) > 20.0);
                    }
                    self.theta = theta;
                    changed = true;
                    self.partials.clear();
                    self.seeded = false;
                }
                _ if deaths => self.partials.clear(),
                _ => self.finish(),
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::noise::ModelError;
    use crate::map::PlanetMap;
    use crate::protocol::Point;
    use crate::simulation::{ShotObservation, simulate_shot};
    use std::time::Duration;

    fn shots() -> Vec<Shot> {
        let truth = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.0,
                    y: 500.0,
                    radius: 30.0,
                    mass: 2700.0,
                },
                Planet {
                    x: 1200.0,
                    y: 700.0,
                    radius: 30.0,
                    mass: 2700.0,
                },
            ],
        };
        let world = WorldParameters::default();
        [
            ((350.0, 500.0), 0.0),
            ((1050.0, 700.0), 0.0),
            ((900.0, 600.0), 150.0),
        ]
        .into_iter()
        .map(|((x, y), angle)| {
            let start = ShotObservation {
                player_id: 1,
                angle_degrees: angle,
                velocity: 6.0,
                points: vec![Point { x, y }],
            };
            let path = simulate_shot(&truth, &start, world, 400).unwrap();
            Shot::new(
                ShotObservation {
                    points: path.points,
                    ..start
                },
                None,
                world,
            )
        })
        .collect()
    }

    fn slice<'a>(
        cancelled: &'a AtomicBool,
        stats: &'a Stats,
        noise: &'a Noise,
        evaluations: u64,
    ) -> Slice<'a> {
        Slice {
            evaluations,
            deadline: Instant::now() + Duration::from_secs(60),
            threads: 2,
            max_planets: 24,
            cancelled,
            preempt: cancelled,
            stats,
            noise,
        }
    }

    /// A map holding a planet too small for its field (P18's concentric split): the unexplained
    /// windows point at its centre, but no seed may overlap it.
    #[test]
    fn no_seed_overlaps_a_planet_of_the_map() {
        use crate::estimator::tests::{planet, shot_in, shot_with_contact, world_of};
        let truth = world_of(vec![planet(800.0, 500.0, 36.0)]);
        let shots: Vec<Shot> = [
            ((650.0, 500.0), 0.0),
            ((800.0, 650.0), 270.0),
            ((950.0, 520.0), 180.0),
        ]
        .into_iter()
        .map(|(start, angle)| shot_with_contact(&shot_in(&truth, start, angle, 6.0), &[]))
        .collect();
        let cancelled = AtomicBool::new(false);
        let world = WorldParameters::default();
        let joint = Fit::new(&shots, world, &cancelled, true, vec![]);
        let map = [[800.0, 500.0, 25.0]];
        let unexplained = joint.unexplained(&map);
        assert!(unexplained[0].significance > SEED_SIGNIFICANCE);
        let towards_centre = unexplained.iter().take(20).all(|w| {
            let (dx, dy) = (800.0 - w.position[0], 500.0 - w.position[1]);
            dx * w.acceleration[0] + dy * w.acceleration[1] > 0.0
        });
        assert!(towards_centre);
        let seeds = seeds(&joint, &map, world);
        assert!(seeds.iter().all(|s| !overlapping(s, &map[0])), "{seeds:?}");
    }

    /// A path that arrives mid-search is absorbed: the pending candidates survive it and the
    /// search has work again.
    #[test]
    fn a_new_path_keeps_pending_work() {
        let (cancelled, stats) = (AtomicBool::new(false), Stats::default());
        let noise = Noise::new(ModelError::PRIOR, &[], &shots(), Some(24));
        let shots = shots();
        let mut search = Search::new();
        search.add_path(&shots[0], &slice(&cancelled, &stats, &noise, 20));
        search.add_path(&shots[1], &slice(&cancelled, &stats, &noise, 20));
        // Enough work for seeds and alone fits, not for a joint refit to settle.
        search.step(&slice(&cancelled, &stats, &noise, 20));
        assert!(!search.candidates.is_empty() && !search.idle());
        let pending = search.candidates.len();
        search.add_path(&shots[2], &slice(&cancelled, &stats, &noise, 20));
        assert_eq!(search.candidates.len(), pending);
        assert!(search.pending);
    }

    /// Slices make progress and the search ends idle with planets found; at the planet cap no
    /// birth is attempted.
    #[test]
    fn slices_grow_the_map_until_idle() {
        let (cancelled, stats) = (AtomicBool::new(false), Stats::default());
        let noise = Noise::new(ModelError::PRIOR, &[], &shots(), Some(24));
        let mut search = Search::new();
        for shot in &shots() {
            search.add_path(shot, &slice(&cancelled, &stats, &noise, 300));
        }
        let mut rounds = 0;
        while !search.idle() && rounds < 200 {
            search.step(&slice(&cancelled, &stats, &noise, 300));
            rounds += 1;
        }
        assert!(search.idle(), "not idle after {rounds} slices");
        assert!(!search.theta.is_empty());

        let mut capped = Search::new();
        for shot in &shots() {
            capped.add_path(shot, &slice(&cancelled, &stats, &noise, 300));
        }
        let mut limited = slice(&cancelled, &stats, &noise, 300);
        limited.max_planets = 0;
        capped.step(&limited);
        assert!(capped.idle() && capped.theta.is_empty());
    }
}
