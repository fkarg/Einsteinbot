//! Diagnosis of why the greedy search misses the truth basin at exact noise (ignored tests that
//! print; run with `cargo test --release --lib diagnose -- --ignored --nocapture --test-threads=1`).
//! Truth is used only to score and to build perturbed starts for the basin probe, never inside the
//! production search. Synthetic worlds follow the W7 generator (24 non-overlapping planets, 8
//! long paths at speed 8) with seeds that are not the scorecard cases; live cases are captures
//! 33, 36 and 40 (the scorecard cases are 12/18/44/49, 52 is held out). Contacts are left out of
//! the fit here (the live `Fit` adds them; they only add surface terms).
use super::fit::{Budget, Fit, planet_at, theta_of};
use super::search::{Limits, Run, best, births, mode_at, seeds};
use super::work::{PartCeilings, Stats};
use super::*;
use crate::event_store::EventLog;
use crate::protocol::{Planet, RecordedEvent};
use crate::simulation::simulate_shot;
use std::time::Instant;

struct Case {
    name: String,
    truth: Vec<Planet>,
    shots: Vec<Shot>,
}

fn uniform(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

fn shot_with_noise(obs: ShotObservation, noise: f64) -> Shot {
    let path = PathData::new(
        &obs.points,
        obs.angle_degrees,
        obs.velocity,
        WorldParameters::default(),
        &[noise],
    );
    Shot {
        obs,
        contact: None,
        path,
    }
}

fn synthetic(seed: u64, k: usize, paths: usize, noise: f64) -> Case {
    let world = WorldParameters::default();
    let mut state = seed;
    let mut truth: Vec<Planet> = Vec::new();
    while truth.len() < k {
        let radius = 20.0 + 20.0 * uniform(&mut state);
        let c = Planet {
            x: radius + (world.battlefield_width - 2.0 * radius) * uniform(&mut state),
            y: radius + (world.battlefield_height - 2.0 * radius) * uniform(&mut state),
            radius,
            mass: radius.powi(3) / 10.0,
        };
        if truth
            .iter()
            .all(|p| (p.x - c.x).hypot(p.y - c.y) > p.radius + c.radius)
        {
            truth.push(c);
        }
    }
    let map = PlanetMap {
        planets: truth.clone(),
    };
    let mut shots = Vec::new();
    while shots.len() < paths {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: (360.0 * uniform(&mut state)).floor(),
            velocity: 8.0,
            points: vec![Point {
                x: (100.0 + (world.battlefield_width - 200.0) * uniform(&mut state)) as f32,
                y: (100.0 + (world.battlefield_height - 200.0) * uniform(&mut state)) as f32,
            }],
        };
        let path = simulate_shot(&map, &shot, world, 2000).unwrap();
        if path.points.len() < 40 {
            continue;
        }
        shots.push(shot_with_noise(
            ShotObservation {
                points: path.points,
                ..shot
            },
            noise,
        ));
    }
    Case {
        name: format!("synthetic seed {seed} K={k} paths={paths}"),
        truth,
        shots,
    }
}

/// The first `paths` distinct paths of the first epoch of a live capture that discloses planets.
fn live(capture: i64, paths: usize, noise: f64) -> Case {
    let events = EventLog::read_capture("einsteinbot.sqlite3", capture).unwrap();
    let mut truth = Vec::new();
    let mut seen: Vec<ShotObservation> = Vec::new();
    for stored in events {
        let RecordedEvent::Inbound(event) = stored.event else {
            continue;
        };
        match &event {
            ServerEvent::Planets { planets } if truth.is_empty() => truth = planets.clone(),
            // Only the first map epoch.
            ServerEvent::Planets { .. } => break,
            ServerEvent::ShotFin { .. } if !truth.is_empty() && seen.len() < paths => {
                let obs = ShotObservation::try_from(&event).unwrap();
                if !seen.contains(&obs) {
                    seen.push(obs);
                }
            }
            _ => {}
        }
    }
    Case {
        name: format!("live {capture} paths={}", seen.len()),
        truth,
        shots: seen
            .into_iter()
            .map(|obs| shot_with_noise(obs, noise))
            .collect(),
    }
}

fn rows(case: &Case) -> usize {
    case.shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .map(|p| 2 * p.row_count())
        .sum()
}

fn nodes(case: &Case) -> usize {
    case.shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .map(|p| p.nodes.len())
        .sum()
}

fn theta_of_planets(planets: &[Planet]) -> Vec<[f64; 3]> {
    planets.iter().map(theta_of).collect()
}

/// The cases the diagnoses run on. `SEEDS` (default 7001,7002,7003) and `KS` (default 24) choose
/// the synthetic worlds, `LIVE` (default 40,36,33) the live captures (`-` for none), `PATHS`
/// the number of paths (default 8 synthetic, 9 live).
fn cases(noise: f64) -> Vec<Case> {
    let list = |name: &str, default: &str| -> Vec<i64> {
        std::env::var(name)
            .unwrap_or_else(|_| default.into())
            .split(',')
            .filter_map(|v| v.parse().ok())
            .collect()
    };
    let paths = list("PATHS", "8")[0] as usize;
    let mut out = Vec::new();
    for k in list("KS", "24") {
        for seed in list("SEEDS", "7001,7002,7003") {
            out.push(synthetic(seed as u64, k as usize, paths, noise));
        }
    }
    for capture in list("LIVE", "40,36,33") {
        out.push(live(capture, paths + 1, noise));
    }
    out
}

/// (a) Cost of one evaluation against rows, nodes and planets (single thread).
#[test]
#[ignore]
fn diagnose_evaluation_cost() {
    let never = AtomicBool::new(false);
    for case in cases(0.0) {
        let fit = Fit::new(
            &case.shots,
            WorldParameters::default(),
            &never,
            false,
            vec![],
        );
        for k in [1, 6, 12, 24] {
            let theta = theta_of_planets(&case.truth[..k.min(case.truth.len())]);
            let time = |jacobian: bool| {
                let started = Instant::now();
                let reps = 20;
                for _ in 0..reps {
                    fit.evaluate(&theta, jacobian, 0, 1).unwrap();
                }
                started.elapsed().as_secs_f64() * 1e3 / reps as f64
            };
            println!(
                "{}: rows {} nodes {} K={} residual {:.3} ms jacobian {:.3} ms",
                case.name,
                rows(&case),
                nodes(&case),
                theta.len(),
                time(false),
                time(true)
            );
        }
    }
}

fn chi2_per_row(fit: &Fit, theta: &[[f64; 3]], rows: usize) -> f64 {
    fit.chi2(theta).unwrap() / rows as f64
}

/// Planets whose removal from the truth raises chi2 by more than 30 (the data constrain them).
fn supported(fit: &Fit, truth: &[[f64; 3]]) -> Vec<usize> {
    let full = fit.chi2(truth).unwrap();
    (0..truth.len())
        .filter(|i| {
            let mut rest = truth.to_vec();
            rest.remove(*i);
            fit.chi2(&rest).unwrap() - full > 30.0
        })
        .collect()
}

/// (b) Is the truth basin reachable, and from how far? Truth perturbed by `delta` world units in
/// each centre (random direction) and `delta / 5` in the radius, then refined (cap 3000
/// evaluations).
#[test]
#[ignore]
fn diagnose_basin_from_perturbed_truth() {
    let never = AtomicBool::new(false);
    for noise in std::env::var("NOISES")
        .unwrap_or_else(|_| "0,0.1".into())
        .split(',')
        .map(|v| v.parse::<f64>().unwrap())
    {
        for case in cases(noise) {
            let fit = Fit::new(
                &case.shots,
                WorldParameters::default(),
                &never,
                false,
                vec![],
            );
            let truth = theta_of_planets(&case.truth);
            let rows = rows(&case);
            let sup = supported(&fit, &truth);
            println!(
                "noise {noise} {}: truth chi2/row {:.2}, {} of {} planets supported",
                case.name,
                chi2_per_row(&fit, &truth, rows),
                sup.len(),
                truth.len()
            );
            let mut state = 99;
            for delta in [0.5, 1.0, 2.0, 5.0, 10.0, 20.0] {
                let start: Vec<[f64; 3]> = truth
                    .iter()
                    .map(|t| {
                        let a = std::f64::consts::TAU * uniform(&mut state);
                        let sign = if uniform(&mut state) < 0.5 { -1.0 } else { 1.0 };
                        [
                            t[0] + delta * a.cos(),
                            t[1] + delta * a.sin(),
                            (t[2] + sign * delta / 5.0).clamp(20.5, 39.5),
                        ]
                    })
                    .collect();
                let started = Instant::now();
                let stats = Stats::default();
                let counted = Fit::new(
                    &case.shots,
                    WorldParameters::default(),
                    &never,
                    false,
                    vec![],
                )
                .with_stats(&stats);
                let budget = Budget {
                    evaluations: 3000,
                    deadline: None,
                    threads: 1,
                };
                let (theta, chi2) = counted.refine(&start, &budget).unwrap();
                let recovered = sup
                    .iter()
                    .filter(|i| {
                        (theta[**i][0] - truth[**i][0]).hypot(theta[**i][1] - truth[**i][1]) < 1.0
                            && (theta[**i][2] - truth[**i][2]).abs() < 0.5
                    })
                    .count();
                println!(
                    "  delta {delta:>4}: start chi2/row {:>10.3e} -> {:>10.3e}; supported planets recovered {recovered}/{}; {} evals, {:.0} ms",
                    chi2_per_row(&fit, &start, rows),
                    chi2 / rows as f64,
                    sup.len(),
                    stats.evaluations(),
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
        }
    }
}

fn nearest(truth: &[[f64; 3]], p: &[f64; 3]) -> (usize, f64) {
    truth
        .iter()
        .enumerate()
        .map(|(i, t)| (i, (t[0] - p[0]).hypot(t[1] - p[1])))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

/// (c) and (d): the greedy growth from the flat model at exact noise, round by round: seed quality
/// (distance to the nearest true planet before and after the single-planet fit), the joint refit
/// against the truth substituted into its matched planets, and the evidence trajectory.
#[test]
#[ignore]
fn diagnose_greedy_growth() {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let noise: f64 = std::env::var("NOISE").map_or(0.0, |v| v.parse().unwrap());
    for case in cases(noise) {
        let rows = rows(&case);
        let truth = theta_of_planets(&case.truth);
        let stats = Stats::default();
        let limits = Limits {
            max_planets: 24,
            deadline: Instant::now() + Duration::from_secs(3600),
            max_evaluations: Some(40_000),
            parallel_evaluations: false,
            ceilings: &PartCeilings::default(),
        };
        let run = Run {
            limits: &limits,
            stats: &stats,
        };
        let joint = Fit::new(&case.shots, world, &never, false, vec![]).with_stats(&stats);
        let sup = supported(&joint, &truth);
        println!(
            "== noise {noise} {}: rows {rows}, truth chi2/row {:.2}, supported planets {}",
            case.name,
            chi2_per_row(&joint, &truth, rows),
            sup.len()
        );
        let mut modes = vec![mode_at(&joint, &[]).unwrap()];
        for round in 0..24 {
            let parent = best(&modes).clone();
            let theta = theta_of_planets(&parent.planets);
            let started = Instant::now();
            let seeds = seeds(&joint, &theta, world);
            // Seed quality: in the basin of a true planet (supported or not) within 3 units?
            let alone = Fit::new(&case.shots, world, &never, false, parent.planets.clone());
            let (mut near_seed, mut near_fit) = (0, 0);
            let mut report = Vec::new();
            for seed in &seeds {
                let (_, d_seed) = nearest(&truth, seed);
                let fitted = alone
                    .refine(&[*seed], &Budget::UNLIMITED)
                    .map(|(t, _)| t[0])
                    .unwrap_or(*seed);
                let (_, d_fit) = nearest(&truth, &fitted);
                near_seed += usize::from(d_seed < 15.0);
                near_fit += usize::from(d_fit < 3.0);
                report.push((d_seed, d_fit));
            }
            let children = births(&case.shots, &joint, &parent, world, &never, &run);
            let child = children.first();
            let parent_matched = theta.iter().filter(|t| nearest(&truth, t).1 < 2.0).count();
            if let Some(child) = child {
                let child_theta = theta_of_planets(&child.planets);
                let chi2 = chi2_per_row(&joint, &child_theta, rows);
                // The child with every planet that lies within 15 units of a true planet replaced
                // by that planet.
                let substituted: Vec<[f64; 3]> = child_theta
                    .iter()
                    .map(|t| {
                        let (i, d) = nearest(&truth, t);
                        if d < 15.0 { truth[i] } else { *t }
                    })
                    .collect();
                let matched = child_theta
                    .iter()
                    .filter(|t| nearest(&truth, t).1 < 2.0)
                    .count();
                println!(
                    "round {round:>2}: K {:>2} -> {:>2}; seeds {} (within 15 of a true planet {near_seed}, fit within 3 {near_fit}); child chi2/row {chi2:>9.3e}, substituted {:>9.3e}; matched <2 units {matched}/{}; evidence {:>12.1} (parent {:>12.1}); evals so far {} ({:.0} ms)",
                    theta.len(),
                    child.planets.len(),
                    seeds.len(),
                    chi2_per_row(&joint, &substituted, rows),
                    child.planets.len(),
                    child.log_evidence,
                    parent.log_evidence,
                    stats.evaluations(),
                    started.elapsed().as_secs_f64() * 1e3
                );
                if round == 0 || std::env::var("VERBOSE").is_ok() {
                    println!("   seed (d_seed, d_fit) {report:.0?}");
                }
            } else {
                println!(
                    "round {round:>2}: K {} no child (seeds {}, parent matched {parent_matched})",
                    theta.len(),
                    seeds.len()
                );
            }
            let improved = child.is_some_and(|c| c.log_evidence > parent.log_evidence);
            modes.extend(children);
            if !improved {
                break;
            }
        }
        let final_mode = best(&modes);
        let theta: Vec<[f64; 3]> = final_mode.planets.iter().map(theta_of).collect();
        println!(
            "   final K {} chi2/row {:.3e}; planets {:?}",
            theta.len(),
            chi2_per_row(&joint, &theta, rows),
            theta.iter().map(|t| planet_at(*t).radius.round()).count()
        );
    }
}

fn f32_sd(x: f64) -> f64 {
    let a = (x as f32).abs();
    if a == 0.0 || !a.is_finite() {
        return 1e-6;
    }
    (2f64.powi(a.log2().floor() as i32 - 23) / 12f64.sqrt()).max(1e-7)
}

fn quantile(values: &mut [f64], q: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * q) as usize]
}

/// Chi-square of `theta` with each planet's field dropped at nodes farther than `radius` from it.
fn truncated_chi2(case: &Case, theta: &[[f64; 3]], radius: f64) -> f64 {
    let mut total = 0.0;
    for shot in &case.shots {
        let Some(path) = &shot.path else { continue };
        let mut field = [vec![0.0; path.nodes.len()], vec![0.0; path.nodes.len()]];
        for th in theta {
            let mass = 0.1 * th[2].powi(3);
            for (i, p) in path.nodes.iter().enumerate() {
                let (dx, dy) = (th[0] - p[0], th[1] - p[1]);
                let d = dx.hypot(dy).max(1e-9);
                if d <= radius {
                    let magnitude = mass / (d * d);
                    field[0][i] += dx / d * magnitude;
                    field[1][i] += dy / d * magnitude;
                }
            }
        }
        for (axis, field) in field.iter().enumerate() {
            let mut out = vec![0.0; path.row_count()];
            path.residual(axis, 0, field, &mut out);
            total += out.iter().map(|v| v * v).sum::<f64>();
        }
    }
    total
}

/// Influence radius: how far a planet's pull exceeds the rounding noise of a window row, the cost of
/// truncating the field and the Jacobian to it, and the speed of the truncated Jacobian.
#[test]
#[ignore]
fn diagnose_influence_radius() {
    let never = AtomicBool::new(false);
    let all = cases(0.0);
    // Row noise sd (rounding of the three ticks of the second difference), both axes.
    let mut sigma: Vec<f64> = Vec::new();
    for case in &all {
        for shot in &case.shots {
            let t = &shot.obs.points;
            for i in 0..t.len().saturating_sub(3) {
                for axis in 0..2 {
                    let v = |k: usize| {
                        let p = &t[i + k];
                        f32_sd(f64::from(if axis == 0 { p.x } else { p.y })).powi(2)
                    };
                    sigma.push((v(0) + 4.0 * v(1) + v(2)).sqrt());
                }
            }
        }
    }
    let (s10, s50, s90) = (
        quantile(&mut sigma, 0.1),
        quantile(&mut sigma, 0.5),
        quantile(&mut sigma, 0.9),
    );
    println!(
        "row rounding sd p10 {s10:.2e} median {s50:.2e} p90 {s90:.2e} (battlefield diagonal 2054)"
    );
    for radius in [20.0, 25.0, 30.0, 35.0, 40.0] {
        let m = 0.1 * f64::powi(radius, 3);
        let at = |k: f64, s: f64| (m / (k * s)).sqrt();
        let pos = |k: f64, s: f64| (2.0 * m / (k * s)).cbrt();
        let rad = |k: f64, s: f64| (3.0 * m / (radius * k * s)).sqrt();
        println!(
            "r {radius}: pull = 1 sd at {:.0} / {:.0} / {:.0} (sd p90 / median / p10); 0.1 sd at {:.0} (median); d(pull)/d(1 unit of centre) = 1 sd at {:.0}, 0.1 sd at {:.0}; d(pull)/d(1 unit radius) = 0.1 sd at {:.0}",
            at(1.0, s90),
            at(1.0, s50),
            at(1.0, s10),
            at(0.1, s50),
            pos(1.0, s50),
            pos(0.1, s50),
            rad(0.1, s50)
        );
    }
    // Score error of truncating the field.
    for case in all
        .iter()
        .filter(|c| c.name.contains("7001") || c.name.contains("live 40"))
    {
        let truth = theta_of_planets(&case.truth);
        let rows = rows(case);
        let line: Vec<String> = [300.0, 600.0, 1000.0, 1500.0, 3000.0]
            .iter()
            .map(|r| {
                format!(
                    "R={r}: {:.3e}",
                    truncated_chi2(case, &truth, *r) / rows as f64
                )
            })
            .collect();
        println!(
            "{}: truth chi2/row with field truncated: {}",
            case.name,
            line.join(", ")
        );
    }
    // Truncated Jacobian: cost per evaluation and effect on the refit from a perturbed truth.
    for case in all
        .iter()
        .filter(|c| c.name.contains("7001") || c.name.contains("7002"))
    {
        let truth = theta_of_planets(&case.truth);
        let rows = rows(case);
        let mut state = 5;
        let start: Vec<[f64; 3]> = truth
            .iter()
            .map(|t| {
                let a = std::f64::consts::TAU * uniform(&mut state);
                [t[0] + 10.0 * a.cos(), t[1] + 10.0 * a.sin(), t[2]]
            })
            .collect();
        for radius in [
            None,
            Some(1500.0),
            Some(1000.0),
            Some(700.0),
            Some(500.0),
            Some(300.0),
        ] {
            let stats = Stats::default();
            let mut fit = Fit::new(
                &case.shots,
                WorldParameters::default(),
                &never,
                false,
                vec![],
            )
            .with_stats(&stats);
            if let Some(radius) = radius {
                fit = fit.with_jacobian_radius(radius);
            }
            let budget = Budget {
                evaluations: 3000,
                deadline: None,
                threads: 1,
            };
            let started = Instant::now();
            let (_, chi2) = fit.refine(&start, &budget).unwrap();
            let wall = started.elapsed().as_secs_f64() * 1e3;
            let reps = 20;
            let timed = Instant::now();
            for _ in 0..reps {
                fit.evaluate(&truth, true, 0, 1).unwrap();
            }
            println!(
                "{} jacobian radius {radius:?}: refit chi2/row {:.4} in {} evals {wall:.0} ms; one Jacobian {:.3} ms",
                case.name,
                chi2 / rows as f64,
                stats.evaluations(),
                timed.elapsed().as_secs_f64() * 1e3 / reps as f64
            );
        }
    }
}

fn events_of(case: &Case) -> Vec<ServerEvent> {
    case.shots
        .iter()
        .map(|shot| ServerEvent::ShotFin {
            player_id: shot.obs.player_id,
            angle: shot.obs.angle_degrees,
            velocity: shot.obs.velocity,
            points: shot.obs.points.clone(),
        })
        .collect()
}

/// Hypothesis "the budget is simply too small": the whole update from the flat model on all paths
/// at exact noise with an increasing deterministic evaluation limit (no wall-clock limit).
#[test]
#[ignore]
fn diagnose_budget_versus_quality() {
    let never = AtomicBool::new(false);
    let limits: Vec<u64> = std::env::var("LIMITS")
        .unwrap_or_else(|_| "1000,5000,20000,80000".into())
        .split(',')
        .map(|v| v.parse().unwrap())
        .collect();
    for case in cases(0.0) {
        let rows = rows(&case);
        let truth = theta_of_planets(&case.truth);
        let fit = Fit::new(
            &case.shots,
            WorldParameters::default(),
            &never,
            false,
            vec![],
        );
        let sup = supported(&fit, &truth);
        println!(
            "== {}: truth chi2/row {:.2}, supported {}",
            case.name,
            chi2_per_row(&fit, &truth, rows),
            sup.len()
        );
        for limit in &limits {
            let mut estimator = Estimator::default();
            estimator.noise_levels = vec![0.0];
            estimator.time_budget = Duration::from_secs(3600);
            estimator.max_evaluations = Some(*limit);
            estimator.parallel_evaluations = std::env::var("SERIAL").is_err();
            let started = Instant::now();
            let belief = estimator.observe_batch(&events_of(&case), &never).unwrap();
            let wall = started.elapsed().as_secs_f64();
            let theta: Vec<[f64; 3]> = belief.map.planets.iter().map(theta_of).collect();
            let matched = sup
                .iter()
                .filter(|i| {
                    theta.iter().any(|t| {
                        (t[0] - truth[**i][0]).hypot(t[1] - truth[**i][1]) < 2.0
                            && (t[2] - truth[**i][2]).abs() < 1.0
                    })
                })
                .count();
            println!(
                "  limit {limit:>7}: K {:>2}, chi2/row {:>9.3e}, truth planets matched (<2 units, <1 radius) {matched}/{}, limit {:?}, used {} evals, {wall:.1} s",
                theta.len(),
                chi2_per_row(&fit, &theta, rows),
                sup.len(),
                belief.search_limit,
                estimator.last_report().unwrap().evaluations,
            );
        }
    }
}

/// Wall time of one evaluation with `threads` threads for several K.
#[test]
#[ignore]
fn diagnose_parallel_evaluation_speed() {
    let never = AtomicBool::new(false);
    for case in cases(0.0).into_iter().take(1) {
        let fit = Fit::new(
            &case.shots,
            WorldParameters::default(),
            &never,
            false,
            vec![],
        );
        for k in [3, 6, 12, 24] {
            let theta = theta_of_planets(&case.truth[..k]);
            let (r1, j1) = fit.evaluate(&theta, true, 0, 1).unwrap();
            let mut line = format!("{} K={k}:", case.name);
            for threads in [1usize, 2, 4, 8, 16] {
                let time = |jacobian: bool| {
                    let reps = 30;
                    let started = Instant::now();
                    for _ in 0..reps {
                        fit.evaluate(&theta, jacobian, 0, threads).unwrap();
                    }
                    started.elapsed().as_secs_f64() * 1e3 / reps as f64
                };
                let (r, j) = fit.evaluate(&theta, true, 0, threads).unwrap();
                assert!(r == r1 && j == j1, "threads change the numbers");
                line += &format!(
                    "  threads {threads}: res {:.3} ms jac {:.3} ms;",
                    time(false),
                    time(true)
                );
            }
            println!("{line}");
        }
    }
}

/// Where one refit's time goes: evaluations against the optimizer's own linear algebra.
#[test]
#[ignore]
fn diagnose_lm_overhead() {
    let never = AtomicBool::new(false);
    for case in cases(0.0) {
        let truth = theta_of_planets(&case.truth);
        let mut state = 5;
        let start: Vec<[f64; 3]> = truth
            .iter()
            .map(|t| {
                let a = std::f64::consts::TAU * uniform(&mut state);
                [t[0] + 10.0 * a.cos(), t[1] + 10.0 * a.sin(), t[2]]
            })
            .collect();
        let stats = Stats::default();
        let fit = Fit::new(
            &case.shots,
            WorldParameters::default(),
            &never,
            false,
            vec![],
        )
        .with_stats(&stats);
        let budget = Budget {
            evaluations: 600,
            deadline: None,
            threads: 1,
        };
        fit.refine(&start, &budget).unwrap();
        let report = stats.report(&PartCeilings::default());
        let ms = |part: Part| report.part(part).map_or(0.0, |p| p.total_ms);
        let calls = |part: Part| report.part(part).map_or(0, |p| p.calls);
        println!(
            "{}: refit {:.0} ms = residual {:.0} ms ({} calls) + jacobian {:.0} ms ({} calls) + optimizer algebra {:.0} ms",
            case.name,
            ms(Part::LmSolve),
            ms(Part::ResidualEval),
            calls(Part::ResidualEval),
            ms(Part::JacobianEval),
            calls(Part::JacobianEval),
            ms(Part::LmSolve) - ms(Part::ResidualEval) - ms(Part::JacobianEval)
        );
    }
}

/// Does the optimizer's tight default stopping rule waste evaluations creeping along flat
/// directions? Refit from the truth perturbed by `delta`, at several tolerances.
#[test]
#[ignore]
fn diagnose_lm_tolerance() {
    let never = AtomicBool::new(false);
    for (case, delta) in cases(0.0)
        .into_iter()
        .zip([10.0, 10.0, 2.0, 1.0, 2.0, 2.0].into_iter().cycle())
    {
        let truth = theta_of_planets(&case.truth);
        let rows = rows(&case);
        let mut state = 99;
        let start: Vec<[f64; 3]> = truth
            .iter()
            .map(|t| {
                let a = std::f64::consts::TAU * uniform(&mut state);
                [t[0] + delta * a.cos(), t[1] + delta * a.sin(), t[2]]
            })
            .collect();
        for tolerance in [None, Some(1e-6), Some(1e-4), Some(1e-3)] {
            let stats = Stats::default();
            let mut fit = Fit::new(
                &case.shots,
                WorldParameters::default(),
                &never,
                false,
                vec![],
            )
            .with_stats(&stats);
            if let Some(tolerance) = tolerance {
                fit = fit.with_tolerance(tolerance);
            }
            let budget = Budget {
                evaluations: 3000,
                deadline: None,
                threads: 1,
            };
            let started = Instant::now();
            let (theta, chi2) = fit.refine(&start, &budget).unwrap();
            let moved = theta
                .iter()
                .zip(&truth)
                .filter(|(t, g)| (t[0] - g[0]).hypot(t[1] - g[1]) > 1.0)
                .count();
            println!(
                "{} delta {delta} tol {tolerance:?}: chi2/row {:.4}, {} evals, {:.0} ms, planets > 1 unit from truth {moved}",
                case.name,
                chi2 / rows as f64,
                stats.evaluations(),
                started.elapsed().as_secs_f64() * 1e3
            );
        }
    }
}

/// Does a birth work when the parent is right? The parent is the truth without planet `i`; seeds
/// come from its unexplained windows. Reports, per missing planet, the nearest seed, the best
/// single-planet fit against the parent's field, and whether a joint refit of parent plus that
/// fit recovers the truth.
#[test]
#[ignore]
fn diagnose_birth_with_a_true_background() {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    for case in cases(0.0) {
        let rows = rows(&case);
        let truth = theta_of_planets(&case.truth);
        let fit = Fit::new(&case.shots, world, &never, false, vec![]);
        let sup = supported(&fit, &truth);
        let (mut seed_near, mut alone_near, mut joint_ok, mut tried) = (0, 0, 0, 0);
        let mut lines = Vec::new();
        for &i in &sup {
            let mut parent = truth.clone();
            let missing = parent.remove(i);
            let seeds = seeds(&fit, &parent, world);
            let d_seed = seeds
                .iter()
                .map(|s| (s[0] - missing[0]).hypot(s[1] - missing[1]))
                .fold(f64::INFINITY, f64::min);
            let alone = Fit::new(
                &case.shots,
                world,
                &never,
                false,
                parent.iter().map(|t| planet_at(*t)).collect(),
            );
            let mut best: Option<([f64; 3], f64)> = None;
            for seed in &seeds {
                if let Some((t, chi2)) = alone.refine(&[*seed], &Budget::UNLIMITED) {
                    if best.is_none_or(|(_, c)| chi2 < c) {
                        best = Some((t[0], chi2));
                    }
                }
            }
            let Some((found, _)) = best else { continue };
            let d_alone = (found[0] - missing[0]).hypot(found[1] - missing[1]);
            let mut start = parent.clone();
            start.push(found);
            let budget = Budget {
                evaluations: 3000,
                deadline: None,
                threads: 1,
            };
            let (joint, chi2) = fit.refine(&start, &budget).unwrap();
            let recovered = joint
                .iter()
                .filter(|t| (t[0] - missing[0]).hypot(t[1] - missing[1]) < 2.0)
                .count()
                > 0;
            tried += 1;
            seed_near += usize::from(d_seed < 15.0);
            alone_near += usize::from(d_alone < 3.0);
            joint_ok += usize::from(recovered);
            lines.push(format!(
                "i={i} r={:.0}: nearest seed {d_seed:.0}, alone fit {d_alone:.0}, joint {} chi2/row {:.2e}",
                missing[2],
                if recovered { "recovered" } else { "missed" },
                chi2 / rows as f64
            ));
        }
        println!(
            "{}: births against a true {}-planet background: nearest seed < 15 units {seed_near}/{tried}, alone fit < 3 units {alone_near}/{tried}, joint refit recovers it {joint_ok}/{tried}",
            case.name,
            truth.len() - 1
        );
        for line in lines {
            println!("   {line}");
        }
    }
}

/// The path pieces within `radius` of `center` (runs of at least 8 consecutive ticks), as paths of
/// their own.
fn local_shots(case: &Case, center: [f64; 2], radius: f64) -> Vec<Shot> {
    let mut out = Vec::new();
    for shot in &case.shots {
        let points = &shot.obs.points[..shot.obs.points.len() - 1];
        let near =
            |p: &Point| (f64::from(p.x) - center[0]).hypot(f64::from(p.y) - center[1]) <= radius;
        let mut start = None;
        for i in 0..=points.len() {
            match (start, i < points.len() && near(&points[i])) {
                (None, true) => start = Some(i),
                (Some(first), false) => {
                    start = None;
                    // One tick of context before the run (the first row needs it).
                    let first = first.saturating_sub(1);
                    let mut piece: Vec<Point> = points[first..i].to_vec();
                    if piece.len() < 8 {
                        continue;
                    }
                    piece.push(piece.last().unwrap().clone());
                    let (dx, dy) = (
                        f64::from(piece[1].x - piece[0].x),
                        f64::from(piece[1].y - piece[0].y),
                    );
                    out.push(shot_with_noise(
                        ShotObservation {
                            player_id: 1,
                            angle_degrees: (-dy).atan2(dx).to_degrees(),
                            velocity: dx.hypot(dy),
                            points: piece,
                        },
                        0.0,
                    ));
                }
                _ => {}
            }
        }
    }
    out
}

/// Births from the flat model: does fitting each seed against only the path pieces near it (so the
/// far field of the unmodelled planets matters little) land in the basin of a true planet?
#[test]
#[ignore]
fn diagnose_local_births() {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    for case in cases(0.0) {
        let truth = theta_of_planets(&case.truth);
        let fit = Fit::new(&case.shots, world, &never, false, vec![]);
        let seeds = seeds(&fit, &[], world);
        println!("{}: {} seeds from the flat model", case.name, seeds.len());
        for radius in [f64::INFINITY, 400.0, 250.0, 150.0] {
            let (mut near, mut within3, mut total) = (0, 0, 0);
            let mut distances = Vec::new();
            for seed in &seeds {
                let shots = if radius.is_finite() {
                    local_shots(&case, [seed[0], seed[1]], radius)
                } else {
                    case.shots.clone()
                };
                if shots.is_empty() {
                    continue;
                }
                let local = Fit::new(&shots, world, &never, false, vec![]);
                let Some((found, _)) = local.refine(&[*seed], &Budget::UNLIMITED) else {
                    continue;
                };
                let (_, d) = nearest(&truth, &found[0]);
                total += 1;
                near += usize::from(d < 15.0);
                within3 += usize::from(d < 3.0);
                distances.push(d.round());
            }
            println!(
                "  window radius {radius:>6}: fitted planet within 15 units of a true planet {near}/{total}, within 3 units {within3}/{total}; distances {distances:?}"
            );
        }
    }
}

/// The QR-based optimizer of the `levenberg_marquardt` crate against the normal-equations one,
/// from the truth perturbed by `delta`.
#[test]
#[ignore]
fn diagnose_normal_equations_optimizer() {
    let never = AtomicBool::new(false);
    for case in cases(0.0) {
        let truth = theta_of_planets(&case.truth);
        let rows = rows(&case);
        for delta in [2.0, 5.0, 10.0, 20.0] {
            let mut state = 99;
            let start: Vec<[f64; 3]> = truth
                .iter()
                .map(|t| {
                    let a = std::f64::consts::TAU * uniform(&mut state);
                    [t[0] + delta * a.cos(), t[1] + delta * a.sin(), t[2]]
                })
                .collect();
            let mut line = format!("{} delta {delta}:", case.name);
            for normal in [false, true] {
                let stats = Stats::default();
                let mut fit = Fit::new(
                    &case.shots,
                    WorldParameters::default(),
                    &never,
                    false,
                    vec![],
                )
                .with_stats(&stats);
                if normal {
                    fit = fit.with_normal_equations();
                }
                let budget = Budget {
                    evaluations: 3000,
                    deadline: None,
                    threads: 1,
                };
                let started = Instant::now();
                let (theta, chi2) = fit.refine(&start, &budget).unwrap();
                let moved = theta
                    .iter()
                    .zip(&truth)
                    .filter(|(t, g)| (t[0] - g[0]).hypot(t[1] - g[1]) > 1.0)
                    .count();
                line += &format!(
                    "  [{}] chi2/row {:.4}, {} evals, {:.0} ms, {moved} planets >1 unit off;",
                    if normal { "normal" } else { "qr" },
                    chi2 / rows as f64,
                    stats.evaluations(),
                    started.elapsed().as_secs_f64() * 1e3
                );
            }
            println!("{line}");
        }
    }
}

mod baseline;
mod incremental;
mod polish;
mod thinning;
