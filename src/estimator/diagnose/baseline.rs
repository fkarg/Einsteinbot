//! P8: baseline comparison on captures (`docs/estimator-experiments.md`, "P8 results"). For each
//! case it prints `QUAL` lines (the format of `incremental.rs`, tables by `scripts/p8_tables.py`)
//! for three maps built from the same paths: `flat` (K = 0), `prod` (the default `Estimator`, fed
//! path by path with a deterministic per-update evaluation limit, the last belief scored) and
//! `rebuild` (E9 cascade plus the E9c guarded polish, cached like the P6 rebuilds). B* comes from
//! `diagnose_incremental` with the same `LIVE`/`PATHS`. Truth only scores, never fits.
//!
//! Run (release): `cargo test --release --lib diagnose_baseline -- --ignored --nocapture
//! --test-threads=1`. Environment: `LIVE`, `SEEDS`, `PATHS`, `NOISE` as in `cases` (PATHS=60 gives
//! live captures up to 61 paths, as in P6), `THREADS`, `CACHE`, `PROD_EVALS` (evaluation limit of
//! each production update, default 2000), `METHODS` (default `flat,prod,rebuild`).
use super::incremental::{quality, rebuild, world_id};
use super::*;
use crate::estimator::work::{TEST_THREADS, cores};

/// The default estimator, path by path; returns (planets, evaluations, wall s, slowest update s).
fn production(case: &Case, evals: u64) -> (Vec<[f64; 3]>, u64, f64, f64) {
    let mut estimator = Estimator::default();
    estimator.time_budget = Duration::from_secs(24 * 3600);
    estimator.max_evaluations = Some(evals);
    let never = AtomicBool::new(false);
    let (mut total, mut slowest) = (0, 0.0_f64);
    let started = Instant::now();
    for event in events_of(case) {
        let t = Instant::now();
        estimator.observe(&event, &never);
        slowest = slowest.max(t.elapsed().as_secs_f64());
        total += estimator.last_report().map_or(0, |r| r.evaluations);
    }
    let planets = estimator
        .belief()
        .map(|b| theta_of_planets(&b.map.planets))
        .unwrap_or_default();
    (planets, total, started.elapsed().as_secs_f64(), slowest)
}

#[test]
#[ignore]
fn diagnose_baseline() {
    if let Some(t) = std::env::var("THREADS").ok().and_then(|v| v.parse().ok()) {
        TEST_THREADS.store(t, std::sync::atomic::Ordering::Relaxed);
    }
    let noise: f64 = std::env::var("NOISE").map_or(0.0, |v| v.parse().unwrap());
    let evals: u64 = std::env::var("PROD_EVALS").map_or(2000, |v| v.parse().unwrap());
    let methods = std::env::var("METHODS").unwrap_or_else(|_| "flat,prod,rebuild".into());
    for case in cases(noise) {
        let (id, n) = (world_id(&case), case.shots.len());
        println!("WORLD {id} paths={n} threads={}", cores());
        for method in methods.split(',') {
            match method {
                "flat" => println!(
                    "QUAL world={id} cfg=flat n={n} {}",
                    quality(&case, n, &[]).text
                ),
                "prod" => {
                    let (theta, evaluations, wall, slowest) = production(&case, evals);
                    println!(
                        "QUAL world={id} cfg=prod n={n} {} evals={evaluations} wall={wall:.1} slowest={slowest:.1} limit={evals}",
                        quality(&case, n, &theta).text
                    );
                }
                "rebuild" => {
                    let r = rebuild(&case, n, noise);
                    println!(
                        "QUAL world={id} cfg=rebuild n={n} {} evals={} equiv={:.0} wall={:.1}",
                        quality(&case, n, &r.theta).text,
                        r.evals,
                        r.equiv,
                        r.wall
                    );
                }
                other => panic!("unknown method {other}"),
            }
        }
    }
}
