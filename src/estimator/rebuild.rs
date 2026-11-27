//! Planet maps built from scratch from every path: births on thinned rows at growing row caps,
//! then a final births-and-deaths stage on all rows, everything under one frozen noise
//! (`noise.rs`). It is the audit of the maintained map (`audit.rs` runs it on a background
//! thread).
use super::Shot;
use super::fit::{Budget, Fit, theta_of};
use super::noise::{Noise, apply, with_noise};
use super::rows::thin_rows;
use super::search::{DEATH_DEPTH, Limits, Run, best, grow, mode_at, prune, refit};
use super::work::{PartCeilings, Stats};
use crate::simulation::WorldParameters;
use std::sync::atomic::AtomicBool;

/// Row caps of the thinned growth stages, coarse to fine. The stages share one evaluation budget.
const CASCADE: [usize; 3] = [500, 1000, 2000];
const CASCADE_EVALUATIONS: u64 = 28_000;
/// Evaluations of the final stage on all rows with fewer / at least `FINAL_LARGE_PATHS` paths.
const FINAL_EVALUATIONS: (u64, u64) = (8_000, 12_000);
const FINAL_LARGE_PATHS: usize = 32;

/// A map from scratch on all of `exact` (full-row floor shots) under `noise`. `None` when
/// cancelled.
pub(super) fn from_scratch(
    exact: &[Shot],
    noise: &Noise,
    max_planets: usize,
    threads: usize,
    cancelled: &AtomicBool,
) -> Option<Vec<[f64; 3]>> {
    let world = WorldParameters::default();
    let ceilings = PartCeilings::default();
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
    let unlimited = Budget {
        evaluations: u64::MAX,
        deadline: None,
        threads,
    };
    let mut modes = Vec::new();
    for (i, cap) in CASCADE.into_iter().enumerate() {
        let mut shots = thin_rows(exact, cap);
        apply(&mut shots, noise);
        let joint = Fit::new(&shots, world, cancelled, true, vec![]).with_stats(&stats);
        if i == 0 {
            modes = vec![mode_at(&joint, &[])?];
        } else {
            let theta: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();
            modes = vec![mode_at(&joint, &theta)?];
            modes.extend(refit(&joint, &theta, &unlimited));
        }
        grow(&joint, &shots, &mut modes, &run, world);
    }
    let theta: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();

    // All rows: refit, births, then the death descent.
    let budget = if exact.len() >= FINAL_LARGE_PATHS {
        FINAL_EVALUATIONS.1
    } else {
        FINAL_EVALUATIONS.0
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
    let shots = with_noise(exact, noise);
    let joint = Fit::new(&shots, world, cancelled, true, vec![]).with_stats(&stats);
    let mut modes = vec![mode_at(&joint, &theta)?];
    modes.extend(refit(
        &joint,
        &theta,
        &Budget {
            evaluations: budget / 4,
            ..unlimited
        },
    ));
    grow(&joint, &shots, &mut modes, &run, world);
    prune(&joint, &mut modes, &run, DEATH_DEPTH);
    let theta: Vec<[f64; 3]> = best(&modes).planets.iter().map(theta_of).collect();
    (!cancelled.load(std::sync::atomic::Ordering::Relaxed)).then_some(theta)
}
