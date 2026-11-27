//! Planet maps built from scratch from every path: the E9 thinned relative-noise cascade followed
//! by the E9c guarded polish (`docs/estimator-experiments.md`). It is the audit of the maintained
//! map (`audit.rs` runs it on a background thread) and the guarded exact refit that the published
//! map gets.
use super::Shot;
use super::fit::{Budget, Fit, theta_of};
use super::rows::thin_rows;
use super::search::{DEATH_DEPTH, Limits, Run, best, grow, mode_at, prune, refit};
use super::work::{PartCeilings, Stats};
use crate::simulation::WorldParameters;
use std::sync::atomic::AtomicBool;

/// Search stages of the cascade: row cap and relative noise level, coarse to fine. The stages
/// share one evaluation budget.
const CASCADE: [(usize, f64); 3] = [(500, 0.1), (1000, 0.02), (2000, 0.005)];
const CASCADE_EVALUATIONS: u64 = 28_000;
/// Relative noise levels of the polish, on full rows; 0 is the exact likelihood.
const POLISH: [f64; 3] = [0.005, 0.001, 0.0];
/// Evaluations of the polish with fewer / at least `POLISH_LARGE_PATHS` paths.
const POLISH_EVALUATIONS: (u64, u64) = (8_000, 12_000);
const POLISH_LARGE_PATHS: usize = 32;
/// The guarded exact refit moves no planet by more than this ...
const GUARD_DISPLACEMENT: f64 = 3.0;
/// ... unless the result explains the data to within this chi2 per row.
const GUARD_CHI2_PER_ROW: f64 = 1.5;
/// Evaluations of the exact refit of the published map.
const EXACT_REFIT_EVALUATIONS: u64 = 600;

/// Residual rows (both axes) of the shots.
pub(super) fn residual_rows(shots: &[Shot]) -> usize {
    shots
        .iter()
        .filter_map(|shot| shot.path.as_ref())
        .map(|path| 2 * path.row_count())
        .sum()
}

/// Largest distance from a start planet to the nearest planet of `after`.
fn displacement(start: &[[f64; 3]], after: &[[f64; 3]]) -> f64 {
    start
        .iter()
        .map(|s| {
            after
                .iter()
                .map(|t| (t[0] - s[0]).hypot(t[1] - s[1]))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

/// `result` instead of `start` when its exact chi2 is lower and it did not drift: no start planet
/// moved more than `GUARD_DISPLACEMENT`, or the result explains the data. The relative-noise fits
/// leave position errors the exact likelihood punishes; the exact refit removes them, but at the
/// exact noise a start more than a few units off a planet can be pulled into a wrong basin.
fn guard(exact: &Fit, rows: usize, start: &[[f64; 3]], result: Vec<[f64; 3]>) -> Vec<[f64; 3]> {
    match (exact.chi2(start), exact.chi2(&result)) {
        (Some(before), Some(after))
            if after < before
                && (displacement(start, &result) < GUARD_DISPLACEMENT
                    || after / rows as f64 <= GUARD_CHI2_PER_ROW) =>
        {
            result
        }
        _ => start.to_vec(),
    }
}

/// The map after a guarded exact refit of `theta` on all rows of `exact` (at most
/// `EXACT_REFIT_EVALUATIONS` evaluations).
pub(super) fn exact_refit(
    exact: &Fit,
    rows: usize,
    theta: &[[f64; 3]],
    threads: usize,
) -> Vec<[f64; 3]> {
    let budget = Budget {
        evaluations: EXACT_REFIT_EVALUATIONS,
        deadline: None,
        threads,
    };
    match exact.refine(theta, &budget) {
        Some((result, _)) => guard(exact, rows, theta, result),
        None => theta.to_vec(),
    }
}

/// A map from scratch on all of `exact` (full-row exact shots): the cascade, then the polish.
/// `None` when cancelled.
pub(super) fn from_scratch(
    exact: &[Shot],
    max_planets: usize,
    threads: usize,
    cancelled: &AtomicBool,
) -> Option<Vec<[f64; 3]>> {
    let world = WorldParameters::default();
    let ceilings = PartCeilings::default();
    let relative = |level: f64| -> Vec<Shot> {
        exact
            .iter()
            .map(|shot| shot.at_relative_level(world, level))
            .collect()
    };
    let stats = Stats::default();
    let limits = Limits {
        max_planets,
        max_evaluations: Some(CASCADE_EVALUATIONS),
        threads,
        ceilings: &ceilings,
    };
    let run = Run {
        limits: &limits,
        stats: &stats,
        cancelled,
    };
    let mut modes = Vec::new();
    for (i, (cap, level)) in CASCADE.into_iter().enumerate() {
        let shots = thin_rows(&relative(level), cap);
        let joint = Fit::new(&shots, world, cancelled, true, vec![]).with_stats(&stats);
        if i == 0 {
            modes = vec![mode_at(&joint, &[])?];
        } else {
            let theta: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();
            let budget = Budget {
                evaluations: u64::MAX,
                deadline: None,
                threads,
            };
            modes = vec![mode_at(&joint, &theta)?];
            modes.extend(refit(&joint, &theta, &budget));
        }
        grow(&joint, &shots, &mut modes, &run, world);
    }
    let mut theta: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();

    let budget = if exact.len() >= POLISH_LARGE_PATHS {
        POLISH_EVALUATIONS.1
    } else {
        POLISH_EVALUATIONS.0
    };
    let stats = Stats::default();
    let limits = Limits {
        max_evaluations: Some(budget),
        ..limits
    };
    let run = Run {
        limits: &limits,
        stats: &stats,
        cancelled,
    };
    let exact_fit = Fit::new(exact, world, cancelled, true, vec![]);
    let rows = residual_rows(exact);
    for level in POLISH {
        let relative_shots = (level > 0.0).then(|| relative(level));
        let shots = relative_shots.as_deref().unwrap_or(exact);
        let joint = Fit::new(shots, world, cancelled, true, vec![]).with_stats(&stats);
        let left = budget.saturating_sub(stats.evaluations());
        let budget = Budget {
            evaluations: left.max(500),
            deadline: None,
            threads,
        };
        let mut modes = vec![mode_at(&joint, &theta)?];
        modes.extend(refit(&joint, &theta, &budget));
        if level > 0.0 {
            grow(&joint, shots, &mut modes, &run, world);
            prune(&joint, &mut modes, &run, DEATH_DEPTH);
        }
        let result: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();
        theta = if level > 0.0 {
            result
        } else {
            guard(&exact_fit, rows, &theta, result)
        };
    }
    (!cancelled.load(std::sync::atomic::Ordering::Relaxed)).then_some(theta)
}
