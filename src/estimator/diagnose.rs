//! Diagnosis harnesses (ignored tests that print; run with
//! `cargo test --release --lib diagnose -- --ignored --nocapture --test-threads=1`). Truth is used
//! only to score, never inside the estimator. Synthetic worlds follow the W7 generator (24
//! non-overlapping planets, long paths at speed 8) with seeds that are not the scorecard cases;
//! live cases are captures with disclosed planets (the scorecard cases 12/18/44/49 and capture 52
//! are reserved; `docs/estimator-experiments.md`). The replayed paths carry no contacts.
//!
//! Environment: `SEEDS` (default 7001,7002,7003) and `KS` (default 24) choose the synthetic worlds,
//! `LIVE` (default 40,36,33) the live captures (`-` for none), `PATHS` (default 8) the number of
//! paths (live captures get one more), `THREADS` the thread limit, `CHECKPOINTS` the path counts
//! that are scored (default 4,8,16,32,60 and the last).
//!
//! Output lines are `TAG key=value ...` (`WORLD`, `UPD`, `QUAL`, `TL`, `TLEND`) for
//! `scripts/p8_tables.py`, `scripts/p6_tables.py` and `scripts/p7_tables.py`.
mod baseline;
mod replay;

use super::fit::{Fit, planet_at, theta_of};
use super::work::TEST_THREADS;
use super::*;
use crate::event_store::EventLog;
use crate::protocol::{Planet, RecordedEvent};
use crate::simulation::simulate_shot;

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

fn exact_shot(obs: ShotObservation) -> Shot {
    Shot::new(obs, None, WorldParameters::default(), 0.0)
}

fn synthetic(seed: u64, k: usize, paths: usize) -> Case {
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
        shots.push(exact_shot(ShotObservation {
            points: path.points,
            ..shot
        }));
    }
    Case {
        name: format!("synthetic seed {seed} K={k} paths={paths}"),
        truth,
        shots,
    }
}

/// The first `paths` distinct paths of the first epoch of a live capture that discloses planets.
fn live(capture: i64, paths: usize) -> Case {
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

/// A world's name without the path count, as a table key.
fn world_id(case: &Case) -> String {
    let name = case.name.split(" paths=").next().unwrap();
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn event_of(shot: &Shot) -> ServerEvent {
    ServerEvent::ShotFin {
        player_id: shot.obs.player_id,
        angle: shot.obs.angle_degrees,
        velocity: shot.obs.velocity,
        points: shot.obs.points.clone(),
    }
}

fn distance(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn acceleration(theta: &[[f64; 3]], p: [f64; 2]) -> [f64; 2] {
    let mut a = [0.0; 2];
    for t in theta {
        let mass = planet_at(*t).mass;
        let (dx, dy) = (t[0] - p[0], t[1] - p[1]);
        let d2 = (dx * dx + dy * dy).max(1e-12);
        let d = d2.sqrt();
        a[0] += dx / d * mass / d2;
        a[1] += dy / d * mass / d2;
    }
    a
}

/// RMS acceleration error of `theta` against `truth` on a 20-unit grid, over grid points within
/// 30 units of a visited row (explored), the others (unexplored) and all (whole); points inside a
/// true planet are left out.
fn field_rmse(shots: &[Shot], theta: &[[f64; 3]], truth: &[[f64; 3]]) -> (f64, f64, f64) {
    let world = WorldParameters::default();
    let visited: Vec<[f64; 2]> = shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .flat_map(|p| (0..p.row_count()).map(|r| p.center(r)))
        .collect();
    let mut sums = [(0.0, 0usize); 2];
    let mut whole = (0.0, 0usize);
    for i in 0..(world.battlefield_width / 20.0) as usize {
        for j in 0..(world.battlefield_height / 20.0) as usize {
            let p = [10.0 + 20.0 * i as f64, 10.0 + 20.0 * j as f64];
            if truth
                .iter()
                .any(|t| (t[0] - p[0]).hypot(t[1] - p[1]) < t[2])
            {
                continue;
            }
            let explored = visited
                .iter()
                .any(|v| (v[0] - p[0]).hypot(v[1] - p[1]) < 30.0);
            let (a, b) = (acceleration(theta, p), acceleration(truth, p));
            let e = (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2);
            let s = &mut sums[usize::from(!explored)];
            s.0 += e;
            s.1 += 1;
            whole.0 += e;
            whole.1 += 1;
        }
    }
    let rms = |s: (f64, usize)| (s.0 / s.1.max(1) as f64).sqrt();
    (rms(sums[0]), rms(sums[1]), rms(whole))
}

/// The scores of `theta` against the truth and the exact likelihood on the first `n` paths, as the
/// fields of a `QUAL` line.
fn quality(case: &Case, n: usize, theta: &[[f64; 3]]) -> String {
    let never = AtomicBool::new(false);
    let shots = &case.shots[..n];
    let rows = rebuild::residual_rows(shots);
    let truth = theta_of_planets(&case.truth);
    let exact = Fit::new(shots, WorldParameters::default(), &never, false, vec![]);
    let chi2_per_row = |t: &[[f64; 3]]| exact.chi2(t).unwrap() / rows as f64;
    let ratio = chi2_per_row(theta) / chi2_per_row(&truth);
    let hit = |p: &[f64; 3], limit: f64| theta.iter().any(|t| distance(t, p) < limit);
    let recall = |limit: f64| truth.iter().filter(|p| hit(p, limit)).count();
    let spurious = theta
        .iter()
        .filter(|t| !truth.iter().any(|p| distance(t, p) < 15.0))
        .count();
    let (fe, fu, fw) = field_rmse(shots, theta, &truth);
    let (fe0, fu0, fw0) = field_rmse(shots, &[], &truth);
    format!(
        "kt={} k={} rec2={} rec15={} spur={spurious} ratio={ratio:.4e} fe={fe:.4e} fu={fu:.4e} fw={fw:.4e} fe0={fe0:.4e} fu0={fu0:.4e} fw0={fw0:.4e}",
        truth.len(),
        theta.len(),
        recall(2.0),
        recall(15.0),
    )
}

fn checkpoints(total: usize) -> Vec<usize> {
    let mut v: Vec<usize> = env_list("CHECKPOINTS", "4,8,16,32,60")
        .into_iter()
        .filter(|&n| n <= total)
        .collect();
    if !v.contains(&total) {
        v.push(total);
    }
    v
}

fn belief_theta(estimator: &Estimator) -> Vec<[f64; 3]> {
    estimator
        .belief()
        .map(|b| theta_of_planets(&b.map.planets))
        .unwrap_or_default()
}

/// Lag-1 autocorrelation of the whitened residuals along each path and chi2 per row, for the true
/// map and the true map minus `DROP` planets, at the search's relative levels. Tests whether the
/// row-independent relative noise over-counts a smooth model error.
#[test]
#[ignore]
fn diagnose_residual_correlation() {
    let drop: usize = std::env::var("DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let world = WorldParameters::default();
    for case in cases() {
        let truth = theta_of_planets(&case.truth);
        // Drop the planets nearest the battlefield centre (a deterministic choice).
        let mut order: Vec<usize> = (0..truth.len()).collect();
        let c = [
            world.battlefield_width / 2.0,
            world.battlefield_height / 2.0,
        ];
        order.sort_by(|&a, &b| {
            (truth[a][0] - c[0])
                .hypot(truth[a][1] - c[1])
                .total_cmp(&(truth[b][0] - c[0]).hypot(truth[b][1] - c[1]))
        });
        let partial: Vec<[f64; 3]> = order[drop..].iter().map(|&i| truth[i]).collect();
        for relative in [0.0, 0.02, 0.005] {
            let shots: Vec<Shot> = case
                .shots
                .iter()
                .map(|s| s.at_relative_level(world, relative))
                .collect();
            let cancelled = AtomicBool::new(false);
            let fit = Fit::new(&shots, world, &cancelled, false, Vec::new());
            for (label, theta) in [("true", &truth), ("missing", &partial)] {
                let (mut num, mut den, mut chi2, mut rows) = (0.0, 0.0, 0.0, 0usize);
                for (i, shot) in shots.iter().enumerate() {
                    let Some(path) = &shot.path else { continue };
                    let field = fit.field(i, path, theta);
                    for axis in 0..2 {
                        let mut r = vec![0.0; path.row_count()];
                        path.residual(axis, &field[axis], &mut r);
                        for w in r.windows(2) {
                            num += w[0] * w[1];
                        }
                        den += r.iter().map(|v| v * v).sum::<f64>();
                        chi2 += r.iter().map(|v| v * v).sum::<f64>();
                        rows += r.len();
                    }
                }
                println!(
                    "RESCORR case={:?} relative={relative} map={label} rows={rows} chi2_per_row={:.3e} lag1={:.3}",
                    case.name,
                    chi2 / rows as f64,
                    num / den
                );
            }
        }
    }
}
