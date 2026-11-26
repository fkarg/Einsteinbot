//! Search over the number of planets K.
//!
//! Every update refits the previous update's retained modes jointly on all paths, then tries to
//! grow and shrink the best one on the evidence of the windowed comparison:
//! - Birth: seed one extra planet where the observed windows' unexplained mean acceleration is
//!   most significant (the residual points toward the missing mass, whose distance follows from
//!   `|a| = mass / d^2` for a few radii; several diverse seeds, no grid). Each seed is first
//!   fitted alone against the field the current planets leave over (cheap), then the best few
//!   are refitted jointly with every planet on every path.
//! - Death: drop each of the weakest planets, refit the rest jointly and compare evidence.
//!
//! Contacts never create planets: they enter the joint fit only as gated surface constraints.
use super::SearchLimit;
use super::Shot;
use super::fit::{Budget, Evidence, Fit, planet_at, theta_of};
use super::work::{Part, PartCeilings, Stats, cores, par_map};
use crate::protocol::Planet;
use crate::simulation::WorldParameters;
use nalgebra::DMatrix;
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
/// Levels of the death descent (planets removed one by one from the best mode).
const DEATH_DEPTH: usize = 6;
/// Modes closer than this in centre and radius are the same planet.
const SAME_PLANET: (f64, f64) = (10.0, 3.0);

#[derive(Clone, Debug)]
pub(super) struct Mode {
    pub planets: Vec<Planet>,
    /// Approximate log evidence including the planet prior; the flat model's is `-0.5 * chi2`.
    pub log_evidence: f64,
    /// `[x, y, radius]` covariance, `None` when under-identified or saturated at a bound.
    pub covariance: Option<DMatrix<f64>>,
}

pub(super) struct Limits<'a> {
    pub max_planets: usize,
    /// Wall-clock bound on the whole search (machine dependent).
    pub deadline: Instant,
    /// Deterministic bound on residual plus Jacobian evaluations. It is checked between search
    /// stages and shared out equally among the refits of a stage, so the result does not depend
    /// on timing or thread scheduling; a stage may overshoot it by the evidence evaluations.
    pub max_evaluations: Option<u64>,
    /// Let a refit use the cores that the other refits of its stage leave idle.
    pub parallel_evaluations: bool,
    pub ceilings: &'a PartCeilings,
}

/// The limits and the work done so far, for sizing the budget of each stage.
pub(super) struct Run<'a> {
    pub limits: &'a Limits<'a>,
    pub stats: &'a Stats,
}

impl Run<'_> {
    fn work_exhausted(&self) -> bool {
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
            threads: if self.limits.parallel_evaluations {
                (cores() / tasks.max(1)).max(1)
            } else {
                1
            },
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

/// Whether two maps are the same mode: the same number of planets, each matching one of the other.
pub(super) fn same_map(a: &[Planet], b: &[Planet]) -> bool {
    a.len() == b.len() && {
        let mut free: Vec<&Planet> = b.iter().collect();
        a.iter().all(|planet| {
            free.iter()
                .position(|other| same_planet(planet, other))
                .map(|i| free.swap_remove(i))
                .is_some()
        })
    }
}

pub(super) fn mode_at(joint: &Fit, theta: &[[f64; 3]]) -> Option<Mode> {
    let Evidence {
        log_evidence,
        covariance,
    } = joint.evidence(theta)?;
    Some(Mode {
        planets: theta.iter().map(|th| planet_at(*th)).collect(),
        log_evidence,
        covariance,
    })
}

pub(super) fn refit(joint: &Fit, start: &[[f64; 3]], budget: &Budget) -> Option<Mode> {
    let (theta, _) = joint.refine(start, budget)?;
    mode_at(joint, &theta)
}

/// Seed planets from the largest unexplained windows of the paths under `theta`.
pub(super) fn seeds(joint: &Fit, theta: &[[f64; 3]], world: WorldParameters) -> Vec<[f64; 3]> {
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
            seeds.push([
                towards(0).clamp(radius, world.battlefield_width - radius),
                towards(1).clamp(radius, world.battlefield_height - radius),
                radius,
            ]);
        }
    }
    seeds
}

/// Joint refits of `parent` plus one more planet, best evidence first.
pub(super) fn births(
    shots: &[Shot],
    joint: &Fit,
    parent: &Mode,
    world: WorldParameters,
    cancelled: &AtomicBool,
    run: &Run,
) -> Vec<Mode> {
    let stats = run.stats;
    let theta: Vec<[f64; 3]> = parent.planets.iter().map(theta_of).collect();
    let seeds = stats.time(Part::Seeding, || seeds(joint, &theta, world));
    if seeds.is_empty() {
        return Vec::new();
    }
    // Stage 1: the new planet alone against the field the parent leaves over.
    let alone = Fit::new(shots, world, cancelled, false, parent.planets.clone())
        .with_deadline(run.limits.deadline)
        .with_stats(stats);
    let budget = run.budget(seeds.len(), Part::BirthAlone);
    let mut fitted: Vec<([f64; 3], f64)> = stats
        .time(Part::BirthAlone, || {
            par_map(&seeds, cores(), |seed| alone.refine(&[*seed], &budget))
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
            par_map(&distinct, cores(), |planet| {
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
pub(super) fn deaths(joint: &Fit, parent: &Mode, run: &Run) -> Vec<Mode> {
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
    let mut modes: Vec<Mode> = par_map(&weakest, cores(), |(i, _)| {
        refit(joint, &without(*i), &budget)
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

/// All modes evaluated this update (unsorted, possibly coinciding) and why the search stopped
/// early, if it did. `None` when cancelled or when the flat model cannot be evaluated.
pub(super) fn search(
    shots: &[Shot],
    world: WorldParameters,
    cancelled: &AtomicBool,
    warm: &[Vec<Planet>],
    limits: &Limits,
    stats: &Stats,
) -> Option<(Vec<Mode>, Option<SearchLimit>)> {
    let run = Run { limits, stats };
    let joint = Fit::new(shots, world, cancelled, true, Vec::new())
        .with_deadline(limits.deadline)
        .with_stats(stats);
    let starts: Vec<Vec<[f64; 3]>> = warm
        .iter()
        .filter(|planets| !planets.is_empty())
        .map(|planets| planets.iter().map(theta_of).collect())
        .collect();
    // The warm modes enter unrefit first, so a refit that is cut short (deadline) or fails can
    // never lose them or leave the belief worse than the previous update's maps on the new data.
    let mut modes = stats.time(Part::WarmStart, || {
        let mut modes = vec![mode_at(&joint, &[])?];
        modes.extend(starts.iter().filter_map(|start| mode_at(&joint, start)));
        Some(modes)
    })?;
    let budget = run.budget(starts.len(), Part::WarmRefit);
    modes.extend(
        stats
            .time(Part::WarmRefit, || {
                par_map(&starts, cores(), |start| refit(&joint, start, &budget))
            })
            .into_iter()
            .flatten(),
    );
    let mut limit = None;
    loop {
        let parent = best(&modes).clone();
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        if parent.planets.len() >= limits.max_planets {
            limit = Some(SearchLimit::PlanetCap);
            break;
        }
        if Instant::now() >= limits.deadline {
            limit = Some(SearchLimit::TimeBudget);
            break;
        }
        if run.work_exhausted() {
            limit = Some(SearchLimit::WorkBudget);
            break;
        }
        let children = births(shots, &joint, &parent, world, cancelled, &run);
        let improved = children
            .first()
            .is_some_and(|child| child.log_evidence > parent.log_evidence);
        modes.extend(children);
        if !improved {
            break;
        }
    }
    // Deaths: descend from the best mode, always continuing from the best child even when it is
    // worse, because planets that cancel each other (spurious leftovers of earlier, thinner
    // data) only look useful one at a time.
    let mut parent = best(&modes).clone();
    for _ in 0..DEATH_DEPTH {
        if cancelled.load(Ordering::Relaxed) {
            return None;
        }
        if parent.planets.is_empty() {
            break;
        }
        if Instant::now() >= limits.deadline {
            limit = limit.or(Some(SearchLimit::TimeBudget));
            break;
        }
        if run.work_exhausted() {
            limit = limit.or(Some(SearchLimit::WorkBudget));
            break;
        }
        let children = stats.time(Part::Deaths, || deaths(&joint, &parent, &run));
        let Some(next) = children.first().cloned() else {
            break;
        };
        modes.extend(children);
        parent = next;
    }
    (!cancelled.load(Ordering::Relaxed)).then_some((modes, limit))
}
