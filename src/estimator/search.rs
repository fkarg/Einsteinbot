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
use super::fit::{Evidence, Fit, planet_at, theta_of};
use crate::protocol::Planet;
use crate::simulation::WorldParameters;
use nalgebra::DMatrix;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

pub(super) struct Limits {
    pub max_planets: usize,
    pub deadline: Instant,
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

fn par_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(items.len());
    let mut results: Vec<(usize, R)> = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break local;
                        };
                        local.push((index, f(item)));
                    }
                })
            })
            .collect::<Vec<_>>();
        // A panicking worker is a bug, not "nothing found": propagate it.
        handles
            .into_iter()
            .flat_map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, result)| result).collect()
}

fn mode_at(joint: &Fit, theta: &[[f64; 3]]) -> Option<Mode> {
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

fn refit(joint: &Fit, start: &[[f64; 3]]) -> Option<Mode> {
    let (theta, _) = joint.refine(start)?;
    mode_at(joint, &theta)
}

/// Seed planets from the largest unexplained windows of the paths under `theta`.
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
fn births(
    shots: &[Shot],
    joint: &Fit,
    parent: &Mode,
    world: WorldParameters,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> Vec<Mode> {
    let theta: Vec<[f64; 3]> = parent.planets.iter().map(theta_of).collect();
    let seeds = seeds(joint, &theta, world);
    if seeds.is_empty() {
        return Vec::new();
    }
    // Stage 1: the new planet alone against the field the parent leaves over.
    let alone =
        Fit::new(shots, world, cancelled, false, parent.planets.clone()).with_deadline(deadline);
    let mut fitted: Vec<([f64; 3], f64)> = par_map(&seeds, |seed| alone.refine(&[*seed]))
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
    let mut modes: Vec<Mode> = par_map(&distinct, |planet| {
        let mut start = theta.clone();
        start.push(*planet);
        refit(joint, &start)
    })
    .into_iter()
    .flatten()
    .collect();
    modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
    modes
}

/// Joint refits of `parent` without each of its weakest planets, best evidence first.
fn deaths(joint: &Fit, parent: &Mode) -> Vec<Mode> {
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
    let mut modes: Vec<Mode> = par_map(&weakest, |(i, _)| refit(joint, &without(*i)))
        .into_iter()
        .flatten()
        .collect();
    modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
    modes
}

fn best(modes: &[Mode]) -> &Mode {
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
) -> Option<(Vec<Mode>, Option<SearchLimit>)> {
    let joint = Fit::new(shots, world, cancelled, true, Vec::new()).with_deadline(limits.deadline);
    let mut modes = vec![mode_at(&joint, &[])?];
    let starts: Vec<Vec<[f64; 3]>> = warm
        .iter()
        .filter(|planets| !planets.is_empty())
        .map(|planets| planets.iter().map(theta_of).collect())
        .collect();
    // The warm modes enter unrefit first, so a refit that is cut short (deadline) or fails can
    // never lose them or leave the belief worse than the previous update's maps on the new data.
    modes.extend(starts.iter().filter_map(|start| mode_at(&joint, start)));
    modes.extend(
        par_map(&starts, |start| refit(&joint, start))
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
        let children = births(shots, &joint, &parent, world, cancelled, limits.deadline);
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
        let children = deaths(&joint, &parent);
        let Some(next) = children.first().cloned() else {
            break;
        };
        modes.extend(children);
        parent = next;
    }
    (!cancelled.load(Ordering::Relaxed)).then_some((modes, limit))
}
