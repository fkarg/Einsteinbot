//! The production estimator on replayed paths, scored against the truth. Two modes:
//! - deterministic (default): after every path `observe`, then `settle` (the slices and the
//!   polish owed, no wall-clock cut unless `CEIL_MS` is given), checkpoints scored on the
//!   published belief. `AUDIT=1` runs the background audit to completion right after the path
//!   that starts it and adopts it when it is better (`AUDIT` lines), so the audit's effect is
//!   measured without timing noise.
//! - `SCHEDULE=<ms>`: a path arrives every that many ms of wall clock, the estimator takes slices
//!   in between (what the live map's worker thread does), `AUDIT=1` runs the audit on its thread.
//!   Prints the recall timeline (`TL`) and the worst publish lag (`TLEND`): the time from a path's
//!   arrival to its map being published.
//!
//! Environment (besides those of `diagnose.rs`): `NAME` (the `cfg=` label), `SLICES`,
//! `SLICE_EVALS`, `CEIL_MS` (slice wall ceiling, 0 for none; default the production 250),
//! `AUDIT`, `SCHEDULE`.
use super::*;

fn settings() -> Settings {
    let mut settings = Settings::default();
    settings.slices_per_path = env_list("SLICES", &settings.slices_per_path.to_string())[0];
    settings.slice_evaluations =
        env_list("SLICE_EVALS", &settings.slice_evaluations.to_string())[0] as u64;
    if let Some(ms) = env_list("CEIL_MS", "-").first() {
        settings.slice_ceiling = (*ms > 0).then(|| Duration::from_millis(*ms as u64));
    }
    settings.audit_enabled = env_list("AUDIT", "0")[0] == 1;
    settings
}

fn name() -> String {
    std::env::var("NAME").unwrap_or_else(|_| "prod".into())
}

fn recall2(theta: &[[f64; 3]], truth: &[[f64; 3]]) -> (usize, usize) {
    let hit = truth
        .iter()
        .filter(|t| theta.iter().any(|p| distance(t, p) <= 2.0))
        .count();
    let spurious = theta
        .iter()
        .filter(|t| !truth.iter().any(|p| distance(t, p) <= 15.0))
        .count();
    (hit, spurious)
}

fn replay_deterministic(case: &Case, settings: Settings) {
    let (id, name) = (world_id(case), name());
    let total = case.shots.len();
    let cps = checkpoints(total);
    let audit = settings.audit_enabled;
    let mut estimator = Estimator::default();
    estimator.settings = settings;
    let never = AtomicBool::new(false);
    println!(
        "WORLD {id} paths={total} threads={} cfg={name}",
        estimator.settings.threads
    );
    for n in 1..=total {
        let started = Instant::now();
        estimator.observe(&event_of(&case.shots[n - 1]), &never);
        let publish_ms = started.elapsed().as_secs_f64() * 1e3;
        estimator.settle(&never);
        let wall_ms = started.elapsed().as_secs_f64() * 1e3;
        let evals = estimator.last_report().map_or(0, |r| r.evaluations);
        println!(
            "UPD world={id} cfg={name} n={n} k={} evals={evals} publish_ms={publish_ms:.1} wall_ms={wall_ms:.1} s100={:.3} l={:.0} white={:.1e}",
            belief_theta(&estimator).len(),
            estimator.model.scale * 100.0,
            estimator.model.length,
            estimator.model.white
        );
        if audit {
            let before = belief_theta(&estimator);
            let started = Instant::now();
            let adopted = estimator.finish_audit().is_some();
            estimator.settle(&never);
            if started.elapsed() > Duration::from_millis(50) || adopted {
                println!(
                    "AUDIT world={id} cfg={name} n={n} adopted={adopted} k_before={} k_after={} audit_s={:.1}",
                    before.len(),
                    belief_theta(&estimator).len(),
                    started.elapsed().as_secs_f64()
                );
            }
        }
        if cps.contains(&n) {
            println!(
                "QUAL world={id} cfg={name} n={n} {}",
                quality(case, n, &belief_theta(&estimator))
            );
        }
    }
}

/// Prints what a step that took over 100 ms spent its time on (`SLOW` lines): the parts of the
/// update report, longest first.
fn slow_step(estimator: &Estimator, started: Instant, id: &str, n: usize) {
    let ms = started.elapsed().as_secs_f64() * 1e3;
    if ms < 100.0 {
        return;
    }
    let mut parts: Vec<_> = estimator
        .last_report()
        .map(|r| r.parts.iter().filter(|p| p.part != Part::LmSolve).collect())
        .unwrap_or_default();
    parts.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
    let top: Vec<String> = parts
        .iter()
        .take(4)
        .map(|p| format!("{}:{:.0}", p.part.name(), p.total_ms))
        .collect();
    println!("SLOW world={id} n={n} ms={ms:.0} top={}", top.join(","));
}

/// A path every `period` ms; between arrivals the estimator works as the live map's worker does.
fn replay_schedule(case: &Case, settings: Settings, period: u64) {
    let (id, name) = (world_id(case), name());
    let truth = theta_of_planets(&case.truth);
    let total = case.shots.len();
    let mut estimator = Estimator::default();
    estimator.settings = settings;
    let never = AtomicBool::new(false);
    let period = Duration::from_millis(period);
    let t0 = Instant::now();
    println!(
        "WORLD {id} paths={total} threads={} cfg={name} period_ms={}",
        estimator.settings.threads,
        period.as_millis()
    );
    let timeline = |estimator: &Estimator, n: usize, what: &str| {
        let theta = belief_theta(estimator);
        let (hit, spur) = recall2(&theta, &truth);
        println!(
            "TL world={id} cfg={name} period_ms={} n={n} t_ms={:.0} k={} rec2={hit} spur={spur} ev={what}",
            period.as_millis(),
            t0.elapsed().as_secs_f64() * 1e3,
            theta.len()
        );
    };
    let mut worst_lag = 0.0_f64;
    for n in 1..=total {
        let arrival = t0 + period * (n as u32 - 1);
        while Instant::now() < arrival {
            if estimator.search_pending() {
                let started = Instant::now();
                estimator.step(&never);
                slow_step(&estimator, started, &id, n - 1);
                timeline(&estimator, n - 1, "slice");
            } else {
                std::thread::sleep(arrival.saturating_duration_since(Instant::now()));
            }
        }
        let started = Instant::now();
        estimator.observe(&event_of(&case.shots[n - 1]), &never);
        slow_step(&estimator, started, &id, n);
        let lag = arrival.elapsed().as_secs_f64() * 1e3;
        worst_lag = worst_lag.max(lag);
        timeline(&estimator, n, "path");
    }
    // The tail: one more period of slices after the last path.
    let end = t0 + period * total as u32;
    while Instant::now() < end && estimator.search_pending() {
        estimator.step(&never);
        timeline(&estimator, total, "slice");
    }
    println!(
        "TLEND world={id} cfg={name} period_ms={} worst_publish_lag_ms={worst_lag:.0}",
        period.as_millis()
    );
    println!(
        "QUAL world={id} cfg={name} n={total} {}",
        quality(case, total, &belief_theta(&estimator))
    );
}

#[test]
#[ignore]
fn diagnose_replay() {
    let schedule = env_list("SCHEDULE", "0")[0] as u64;
    for case in cases() {
        if schedule > 0 {
            replay_schedule(&case, settings(), schedule);
        } else {
            replay_deterministic(&case, settings());
        }
    }
}
