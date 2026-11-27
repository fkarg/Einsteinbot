//! Diagnosis harnesses (ignored tests that print; run with
//! `cargo test --release --lib diagnose -- --ignored --nocapture --test-threads=1`). Truth is used
//! only to score, never inside the estimator. Synthetic worlds follow the W7 generator
//! (`eval::synthetic_world`) with seeds that are not the scorecard cases; live cases are captures
//! with disclosed planets. The replayed paths carry no contacts. The replay and baseline harnesses
//! moved to `devtool eval` (P14).
//!
//! Environment: `SEEDS` (default 7001,7002,7003) and `KS` (default 24) choose the synthetic worlds,
//! `LIVE` (default 40,36,33) the live captures (`-` for none), `PATHS` (default 8) the number of
//! paths (live captures get one more), `THREADS` the thread limit.
use super::fit::{Fit, planet_at, theta_of};
use super::work::TEST_THREADS;
use super::*;
use crate::event_store::EventLog;
use crate::protocol::{Planet, RecordedEvent};

struct Case {
    name: String,
    truth: Vec<Planet>,
    shots: Vec<Shot>,
}

fn exact_shot(obs: ShotObservation) -> Shot {
    Shot::new(obs, None, WorldParameters::default())
}

fn synthetic(seed: u64, k: usize, paths: usize) -> Case {
    let (truth, shots) = eval::synthetic_world(seed, k, paths);
    Case {
        name: format!("synthetic seed {seed} K={k} paths={paths}"),
        truth,
        shots: shots.into_iter().map(exact_shot).collect(),
    }
}

/// The first `paths` distinct paths of the first epoch of a live capture that discloses planets.
fn live(capture: i64, paths: usize) -> Case {
    let events = EventLog::read_capture("einsteinbot.sqlite3", capture).unwrap();
    // `EPOCH=k` (opt-in): the k-th map epoch, an epoch starting where the disclosed planets
    // change (repeated identical `Planets` events stay in it). Without it, the first epoch up to
    // the next `Planets` event, as before.
    let epoch_wanted = std::env::var("EPOCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok());
    let mut epoch = 0;
    let mut truth = Vec::new();
    let mut seen: Vec<ShotObservation> = Vec::new();
    for stored in events {
        let RecordedEvent::Inbound(event) = stored.event else {
            continue;
        };
        match &event {
            ServerEvent::Planets { planets } if truth.is_empty() => truth = planets.clone(),
            ServerEvent::Planets { planets } if epoch_wanted.is_some() => {
                if *planets != truth {
                    if Some(epoch) == epoch_wanted {
                        break;
                    }
                    epoch += 1;
                    truth = planets.clone();
                    seen.clear();
                }
            }
            // Only the first map epoch.
            ServerEvent::Planets { .. } => break,
            ServerEvent::ShotFin { .. }
                if !truth.is_empty()
                    && seen.len() < paths
                    && epoch_wanted.is_none_or(|e| e == epoch) =>
            {
                let obs = ShotObservation::try_from(&event).unwrap();
                if !seen.contains(&obs) {
                    seen.push(obs);
                }
            }
            _ => {}
        }
    }
    Case {
        name: match epoch_wanted {
            Some(e) => format!("live {capture}e{e} paths={}", seen.len()),
            None => format!("live {capture} paths={}", seen.len()),
        },
        truth,
        shots: seen.into_iter().map(exact_shot).collect(),
    }
}

fn theta_of_planets(planets: &[Planet]) -> Vec<[f64; 3]> {
    planets.iter().map(theta_of).collect()
}

fn env_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect()
}

/// The cases the diagnoses run on (see the module documentation).
fn cases() -> Vec<Case> {
    if let Some(threads) = env_list("THREADS", "0").first() {
        TEST_THREADS.store(*threads, Ordering::Relaxed);
    }
    let paths = env_list("PATHS", "8")[0];
    let mut out = Vec::new();
    for k in env_list("KS", "24") {
        for seed in env_list("SEEDS", "7001,7002,7003") {
            out.push(synthetic(seed as u64, k, paths));
        }
    }
    for capture in env_list("LIVE", "40,36,33") {
        out.push(live(capture as i64, paths + 1));
    }
    out
}

fn belief_theta(estimator: &Estimator) -> Vec<[f64; 3]> {
    estimator
        .belief()
        .map(|b| theta_of_planets(&b.map.planets))
        .unwrap_or_default()
}

/// P13 check of the production likelihood (`noise.rs`): for the true map and the true map minus
/// `DROP` central planets (maps not fitted, so the residuals are honest), the model-error
/// scalars estimated by `noise::estimate` from every path's residuals, and the production
/// innovations' chi2/row and lag-1 autocorrelation along each path. Pass (from the prototype):
/// chi2/row near 1 and |lag-1| below 0.1 for both maps. `MODEL=off` uses the floor alone.
#[test]
#[ignore]
fn diagnose_matern_model_error() {
    use super::noise::{ModelError, Noise, Series, estimate, with_noise};
    let drop: usize = std::env::var("DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let off = std::env::var("MODEL").is_ok_and(|v| v == "off");
    let white = std::env::var("FLOOR").is_ok_and(|v| v == "white");
    let world = WorldParameters::default();
    for mut case in cases() {
        if white {
            // Every row kept, its floor treated as white (the prototype's approximation).
            for shot in &mut case.shots {
                shot.path = shot
                    .path
                    .as_ref()
                    .and_then(|p| p.thinned(&(0..p.row_count()).collect::<Vec<_>>()));
            }
        }
        let truth = theta_of_planets(&case.truth);
        let c = [
            world.battlefield_width / 2.0,
            world.battlefield_height / 2.0,
        ];
        let mut order: Vec<usize> = (0..truth.len()).collect();
        order.sort_by(|&a, &b| {
            (truth[a][0] - c[0])
                .hypot(truth[a][1] - c[1])
                .total_cmp(&(truth[b][0] - c[0]).hypot(truth[b][1] - c[1]))
        });
        let partial: Vec<[f64; 3]> = order[drop..].iter().map(|&i| truth[i]).collect();
        let cancelled = AtomicBool::new(false);
        for (label, theta) in [("true", &truth), ("missing", &partial)] {
            let planets: Vec<Planet> = theta.iter().map(|t| planet_at(*t)).collect();
            let started = Instant::now();
            let mut noise = Noise::new(ModelError::PRIOR, &planets, &case.shots, Some(24));
            let raster_ms = started.elapsed().as_secs_f64() * 1e3;
            let series: Vec<Series> = case
                .shots
                .iter()
                .filter_map(|s| s.path.as_ref())
                .map(|p| Series::new(p, &planets, &noise))
                .collect();
            let started = Instant::now();
            noise.model = if off {
                ModelError {
                    scale: 0.0,
                    ..ModelError::PRIOR
                }
            } else {
                estimate(&series, None)
            };
            let estimate_ms = started.elapsed().as_secs_f64() * 1e3;
            if label == "missing" {
                // Far rows only: rows within `rho0` of a missing planet are near-field signatures
                // left for births; they are missing in both the estimate and the score.
                let missing_planets: Vec<[f64; 3]> = truth
                    .iter()
                    .filter(|t| !partial.contains(t))
                    .copied()
                    .collect();
                let mut far_series = Vec::new();
                for p in case.shots.iter().filter_map(|s| s.path.as_ref()) {
                    let mut series = Series::new(p, &planets, &noise);
                    let near: Vec<bool> = (0..p.row_count())
                        .map(|r| {
                            let c = p.center(r);
                            missing_planets
                                .iter()
                                .any(|m| (m[0] - c[0]).hypot(m[1] - c[1]) < noise.rho0)
                        })
                        .collect();
                    series.drop_rows(&near);
                    far_series.push(series);
                }
                let model = estimate(&far_series, None);
                let (mut chi2, mut used, mut total, mut num, mut den) =
                    (0.0, 0usize, 0usize, 0.0, 0.0);
                for series in &far_series {
                    let missing = series.missing();
                    for (v, _) in series.innovations(model) {
                        for (r, x) in v.iter().enumerate() {
                            total += 1;
                            if !missing[r] {
                                used += 1;
                                chi2 += x * x;
                                den += x * x;
                                if r + 1 < v.len() && !missing[r + 1] {
                                    num += x * v[r + 1];
                                }
                            }
                        }
                    }
                }
                println!(
                    "FAR case={:?} rho0={:.1} s100={:.3} l={:.1} far_rows={used}/{total} ({:.0}%) chi2_per_row={:.3} lag1={:.3}",
                    case.name,
                    noise.rho0,
                    model.scale * 100.0,
                    model.length,
                    100.0 * used as f64 / total as f64,
                    chi2 / used.max(1) as f64,
                    num / den
                );
            }
            if std::env::var("GRID").is_ok() {
                for si in 0..9 {
                    let scale = 0.01 * 2f64.powf(si as f64 * 0.5);
                    let line: Vec<String> = (0..9)
                        .map(|li| {
                            let length = 50.0 * 2f64.powf(li as f64 * 0.5);
                            let model = ModelError {
                                scale,
                                length,
                                ..ModelError::PRIOR
                            };
                            format!(
                                "{:.0}",
                                series.iter().map(|s| s.log_likelihood(model)).sum::<f64>()
                            )
                        })
                        .collect();
                    println!(
                        "GRID map={label} s100={:.2} ll(l=50..800)={}",
                        scale * 100.0,
                        line.join(" ")
                    );
                }
            }
            let shots = with_noise(&case.shots, &noise);
            let fit = Fit::new(&shots, world, &cancelled, false, Vec::new());
            let (mut chi2, mut rows, mut num, mut den) = (0.0, 0usize, 0.0, 0.0);
            for (i, shot) in shots.iter().enumerate() {
                let Some(path) = &shot.path else { continue };
                let field = fit.field(i, path, theta);
                for axis in 0..2 {
                    let mut r = vec![0.0; path.row_count()];
                    path.residual(axis, &field[axis], &mut r);
                    for w in r.windows(2) {
                        num += w[0] * w[1];
                    }
                    let ss: f64 = r.iter().map(|v| v * v).sum();
                    den += ss;
                    chi2 += ss;
                    rows += r.len();
                }
            }
            println!(
                "MATERN case={:?} shape=prod map={label} rho0={:.1} s100={:.3} l={:.1} rows={rows} chi2_per_row={:.3} lag1={:.3} raster_ms={raster_ms:.1} estimate_ms={estimate_ms:.0}",
                case.name,
                noise.rho0,
                noise.model.scale * 100.0,
                noise.model.length,
                chi2 / rows as f64,
                num / den
            );
        }
    }
}

/// Capture `CAPTURE` (default 55), first epoch, through the full event path (`observe` on every
/// recorded inbound event, so contacts are classified with the known players; `settle` after
/// each shot): whether a published planet lies within 2 / 15 units of truth planet `PLANET`
/// (default 6, the viewer's 0-based numbering) at 16, 32, 64, 128 paths and at the end, plus the
/// shots and contacts near it.
#[test]
#[ignore]
fn diagnose_full_event_planet() {
    let capture = env_list("CAPTURE", "55")[0] as i64;
    let index = env_list("PLANET", "6")[0];
    if let Some(threads) = env_list("THREADS", "0").first() {
        TEST_THREADS.store(*threads, Ordering::Relaxed);
    }
    let events = EventLog::read_capture("einsteinbot.sqlite3", capture).unwrap();
    let mut estimator = Estimator::default();
    let never = AtomicBool::new(false);
    let mut truth: Vec<Planet> = Vec::new();
    let (mut planets_events, mut identical, mut shots) = (0, 0, 0usize);
    let checkpoints = [16usize, 32, 64, 128];
    let max_paths = env_list("MAXPATHS", "128")[0];
    let report = |estimator: &Estimator, n: usize, truth: &[Planet]| {
        let t = &truth[index];
        let theta = belief_theta(estimator);
        let nearest = theta
            .iter()
            .map(|p| (p[0] - t.x).hypot(p[1] - t.y))
            .fold(f64::INFINITY, f64::min);
        let contacts = estimator
            .shots
            .iter()
            .filter_map(|s| s.contact.as_ref())
            .filter(|c| {
                ((f64::from(c.point.x) - t.x).hypot(f64::from(c.point.y) - t.y) - t.radius).abs()
                    < 15.0
            })
            .count();
        let near_paths = estimator
            .shots
            .iter()
            .filter(|s| {
                s.obs
                    .points
                    .iter()
                    .any(|p| (f64::from(p.x) - t.x).hypot(f64::from(p.y) - t.y) < t.radius + 60.0)
            })
            .count();
        println!(
            "PLANET capture={capture} index={index} n={n} k={} nearest={nearest:.1} within2={} within15={} contacts_on_surface={contacts} paths_within_r+60={near_paths} truth=({:.0},{:.0},r{:.0})",
            theta.len(),
            nearest <= 2.0,
            nearest <= 15.0,
            t.x,
            t.y,
            t.radius
        );
    };
    for stored in events {
        let RecordedEvent::Inbound(event) = stored.event else {
            continue;
        };
        if let ServerEvent::Planets { planets } = &event {
            planets_events += 1;
            if truth.is_empty() {
                truth = planets.clone();
            } else if *planets == truth {
                // The estimator resets on every `Planets` event; the live map does the same.
                identical += 1;
            } else {
                break;
            }
        }
        let before = estimator.shots.len();
        estimator.observe(&event, &never);
        if estimator.shots.len() > before {
            estimator.settle(&never);
            shots = estimator.shots.len();
            if checkpoints.contains(&shots) {
                report(&estimator, shots, &truth);
            }
            if shots >= max_paths {
                break;
            }
        }
    }
    println!(
        "EPOCH capture={capture} planets_events={planets_events} identical_repeats={identical} paths={shots}"
    );
    report(&estimator, shots, &truth);
}
