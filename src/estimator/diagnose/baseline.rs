//! P8 baseline comparison on captures (`docs/estimator-experiments.md`, "P8 results", "P11
//! results"). For each case it prints `QUAL` lines (tables by `scripts/p8_tables.py`) for three
//! maps built from the same paths: `flat` (K = 0), `prod` (the default `Estimator`, fed path by
//! path and settled, deterministic, the last belief scored) and `rebuild` (the from-scratch
//! cascade and polish that the audit runs). Truth only scores, never fits.
//!
//! Run (release): `cargo test --release --lib diagnose_baseline -- --ignored --nocapture
//! --test-threads=1`. Environment: those of `diagnose.rs`, and `METHODS` (default
//! `flat,prod,rebuild`).
use super::*;
use crate::estimator::rebuild::from_scratch;
use crate::estimator::work::cores;

/// The default estimator, path by path; returns (planets, evaluations, wall s, slowest update s).
fn production(case: &Case) -> (Vec<[f64; 3]>, u64, f64, f64) {
    let mut estimator = Estimator::default();
    // Deterministic: no slice is cut by the wall clock.
    estimator.settings.slice_ceiling = None;
    let never = AtomicBool::new(false);
    let (mut total, mut slowest) = (0, 0.0_f64);
    let started = Instant::now();
    for shot in &case.shots {
        let t = Instant::now();
        estimator.observe(&event_of(shot), &never);
        estimator.settle(&never);
        slowest = slowest.max(t.elapsed().as_secs_f64());
        total += estimator.last_report().map_or(0, |r| r.evaluations);
    }
    (
        belief_theta(&estimator),
        total,
        started.elapsed().as_secs_f64(),
        slowest,
    )
}

#[test]
#[ignore]
fn diagnose_baseline() {
    let methods = std::env::var("METHODS").unwrap_or_else(|_| "flat,prod,rebuild".into());
    for case in cases() {
        let (id, n) = (world_id(&case), case.shots.len());
        println!("WORLD {id} paths={n} threads={}", cores());
        for method in methods.split(',') {
            match method {
                "flat" => println!("QUAL world={id} cfg=flat n={n} {}", quality(&case, n, &[])),
                "prod" => {
                    let (theta, evaluations, wall, slowest) = production(&case);
                    println!(
                        "QUAL world={id} cfg=prod n={n} {} evals={evaluations} wall={wall:.1} slowest={slowest:.1}",
                        quality(&case, n, &theta)
                    );
                }
                "rebuild" => {
                    let never = AtomicBool::new(false);
                    let started = Instant::now();
                    let noise = crate::estimator::noise::NoiseModel::new(
                        crate::estimator::noise::ModelError::PRIOR,
                        &case.shots,
                    );
                    let theta = from_scratch(&case.shots, &noise, 32, cores(), &never)
                        .expect("not cancelled");
                    println!(
                        "QUAL world={id} cfg=rebuild n={n} {} wall={:.1}",
                        quality(&case, n, &theta),
                        started.elapsed().as_secs_f64()
                    );
                }
                other => panic!("unknown method {other}"),
            }
        }
    }
}
