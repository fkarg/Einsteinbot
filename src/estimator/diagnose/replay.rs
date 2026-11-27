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
//! `AUDIT`, `SCHEDULE`, `PLANETS` (known total count; absent: the prior), `CAP` (`max_planets`), `LEARN`
//! (the dynamic planet count of P25 from that start; `count=` on `UPD` lines).
//! `UPD` lines carry the published belief's residual-correction state (`gate`: `on:<max>` or the
//! reason) and the count-limit warning (`cap`, `limited`: the path it was first raised at, P23).
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
    // `PLANETS=n`: the known total count (fixed); absent: the prior. `CAP=n`: the cap without it.
    settings.planets = env_list("PLANETS", "-").first().copied();
    if let Some(cap) = env_list("CAP", "-").first() {
        settings.max_planets = *cap;
    }
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
    // `LEARN=n`: the dynamic planet count (P25) starting from `n`.
    if let Some(start) = env_list("LEARN", "-").first() {
        estimator.settings.learn_planets = true;
        estimator.settings.planets = Some(*start);
        estimator.learner = Some(super::count::Learner::new(*start));
    }
    let never = AtomicBool::new(false);
    // P25 measurement: path coverage (hidden fractions) and the tick distance of missed planets.
    let mut coverage = super::count::Coverage::new(WorldParameters::default());
    let truth = theta_of_planets(&case.truth);
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
        let noise_ms = estimator.last_report().map_or(0.0, |r| {
            r.parts
                .iter()
                .filter(|p| p.part == Part::Noise)
                .map(|p| p.total_ms)
                .sum()
        });
        let belief = estimator.belief();
        let gate = belief.map_or("none".to_string(), |b| {
            match (
                &b.residual_correction,
                b.residual_correction_unavailable_reason,
            ) {
                (Some(c), _) => format!("on:{:.1e}", c.max_magnitude),
                (None, Some(r)) => format!("{r:?}"),
                (None, None) => "none".into(),
            }
        });
        let limit = belief.and_then(|b| b.count_limit.as_ref());
        coverage.cover(&estimator.shots);
        let found = belief.map_or(vec![], |b| b.map.planets.clone());
        let hid: Vec<String> = [60.0, 100.0, 150.0, 200.0, 300.0]
            .iter()
            .map(|r| format!("{:.4}", coverage.hidden_fraction(&found, *r)))
            .collect();
        let theta = belief_theta(&estimator);
        let miss: Vec<String> = truth
            .iter()
            .filter(|t| !theta.iter().any(|p| distance(t, p) <= 15.0))
            .map(|t| format!("{:.0}", coverage.tick_distance([t[0], t[1]])))
            .collect();
        println!(
            "HID world={id} n={n} k={} hid={} miss={}",
            theta.len(),
            hid.join(","),
            if miss.is_empty() {
                "-".into()
            } else {
                miss.join(",")
            }
        );
        println!(
            "UPD world={id} cfg={name} n={n} k={} evals={evals} publish_ms={publish_ms:.1} wall_ms={wall_ms:.1} noise_ms={noise_ms:.1} s100={:.3} l={:.0} white={:.1e} cap={} limited={} count={} gate={gate}",
            belief_theta(&estimator).len(),
            estimator.model.scale * 100.0,
            estimator.model.length,
            estimator.model.white,
            limit.is_some_and(|l| l.at_cap),
            limit
                .and_then(|l| l.detected_at_path)
                .map_or("-".into(), |n| n.to_string()),
            estimator
                .settings
                .planets
                .map_or("-".into(), |n| n.to_string()),
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
    let mut lags = Vec::with_capacity(total);
    let mut max_queued = 0;
    // Serial per-path phases of `observe` (ms) once the epoch has many paths (n > total / 2).
    let mut late: BTreeMap<Part, Vec<f64>> = BTreeMap::new();
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
        let queued = (n..total)
            .take_while(|k| t0 + period * *k as u32 <= started)
            .count();
        max_queued = max_queued.max(queued);
        estimator.observe(&event_of(&case.shots[n - 1]), &never);
        slow_step(&estimator, started, &id, n);
        lags.push(arrival.elapsed().as_secs_f64() * 1e3);
        if 2 * n > total {
            for part in [
                Part::Noise,
                Part::Diagnostics,
                Part::ResidualCorrection,
                Part::WarmRefit,
            ] {
                let ms = estimator
                    .last_report()
                    .and_then(|r| r.part(part))
                    .map_or(0.0, |p| p.total_ms);
                late.entry(part).or_default().push(ms);
            }
        }
        timeline(&estimator, n, "path");
    }
    // The tail: one more period of slices after the last path.
    let end = t0 + period * total as u32;
    while Instant::now() < end && estimator.search_pending() {
        estimator.step(&never);
        timeline(&estimator, total, "slice");
    }
    let quantile = |values: &mut Vec<f64>, q: f64| {
        values.sort_by(f64::total_cmp);
        values
            .get(((values.len() as f64 * q).ceil() as usize).saturating_sub(1))
            .copied()
            .unwrap_or(0.0)
    };
    let worst_lag = quantile(&mut lags, 1.0);
    println!(
        "TLEND world={id} cfg={name} period_ms={} worst_publish_lag_ms={worst_lag:.0} p90_lag_ms={:.0} median_lag_ms={:.0} max_queued={max_queued}",
        period.as_millis(),
        quantile(&mut lags, 0.9),
        quantile(&mut lags, 0.5),
    );
    for (part, mut ms) in late {
        println!(
            "LATE world={id} cfg={name} part={} median_ms={:.1} p90_ms={:.1} max_ms={:.1}",
            part.name(),
            quantile(&mut ms, 0.5),
            quantile(&mut ms, 0.9),
            quantile(&mut ms, 1.0)
        );
    }
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
