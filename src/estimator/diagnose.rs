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

/// Falsification test of a physics-derived model-error term (review F1, tier A): the field of
/// the unfound planets as an Ornstein-Uhlenbeck process along each path, its variance from
/// Campbell's theorem for `N_u` unseen planets uniform on the admissible region U (outside found
/// planets' exclusion disks and the 20-unit no-hit tube around observed ticks) and its length
/// the distance to U. Prints chi2/row and lag-1 autocorrelation of the Kalman innovations for the
/// true map (N_u = 0) and the true map minus `DROP` central planets (N_u = DROP).
#[test]
#[ignore]
fn diagnose_campbell_model_error() {
    let drop: usize = std::env::var("DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let world = WorldParameters::default();
    // E[m^2] for r ~ U[20, 40], m = r^3 / 10.
    let e_m2 = (40f64.powi(7) - 20f64.powi(7)) / (7.0 * 20.0) / 100.0;
    let (w, h) = (world.battlefield_width, world.battlefield_height);
    let diagonal = w.hypot(h);
    for case in cases() {
        let truth = theta_of_planets(&case.truth);
        let c = [w / 2.0, h / 2.0];
        let mut order: Vec<usize> = (0..truth.len()).collect();
        order.sort_by(|&a, &b| {
            (truth[a][0] - c[0])
                .hypot(truth[a][1] - c[1])
                .total_cmp(&(truth[b][0] - c[0]).hypot(truth[b][1] - c[1]))
        });
        let partial: Vec<[f64; 3]> = order[drop..].iter().map(|&i| truth[i]).collect();
        let ticks: Vec<[f64; 2]> = case
            .shots
            .iter()
            .filter_map(|s| s.path.as_ref())
            .flat_map(|p| p.ticks.iter().copied())
            .collect();
        let cancelled = AtomicBool::new(false);
        let fit = Fit::new(&case.shots, world, &cancelled, false, Vec::new());
        for (label, theta, unseen) in [("true", &truth, 0usize), ("missing", &partial, drop)] {
            // Admissible centres of an unseen planet on a 10-unit raster.
            let found: Vec<Planet> = theta.iter().map(|t| planet_at(*t)).collect();
            let mut admissible: Vec<[f64; 2]> = Vec::new();
            let mut y = 25.0;
            while y < h - 20.0 {
                let mut x = 25.0;
                while x < w - 20.0 {
                    let free = found
                        .iter()
                        .all(|p| (p.x - x).hypot(p.y - y) > p.radius + 20.0)
                        && ticks.iter().all(|t| (t[0] - x).hypot(t[1] - y) > 20.0);
                    if free {
                        admissible.push([x, y]);
                    }
                    x += 10.0;
                }
                y += 10.0;
            }
            let area = admissible.len() as f64 * 100.0;
            let lambda = if area > 0.0 {
                unseen as f64 / area
            } else {
                0.0
            };
            let (mut chi2, mut rows, mut num, mut den) = (0.0, 0usize, 0.0, 0.0);
            for (i, shot) in case.shots.iter().enumerate() {
                let Some(path) = &shot.path else { continue };
                let n = path.row_count();
                let field = fit.field(i, path, theta);
                let ones = vec![1.0; path.nodes.len()];
                let mut scale = vec![0.0; n];
                path.apply(&ones, &mut scale, 0..n);
                let centers: Vec<[f64; 2]> = (0..n).map(|r| path.center(r)).collect();
                let distance: Vec<f64> = centers
                    .iter()
                    .map(|p| {
                        admissible
                            .iter()
                            .map(|a| (a[0] - p[0]).hypot(a[1] - p[1]))
                            .fold(f64::INFINITY, f64::min)
                            .max(20.0)
                    })
                    .collect();
                let sigma2: Vec<f64> = (0..n)
                    .map(|r| {
                        let d = distance[r];
                        let v = lambda
                            * e_m2
                            * std::f64::consts::FRAC_PI_2
                            * (1.0 / (d * d) - 1.0 / (diagonal * diagonal));
                        scale[r] * scale[r] * v.max(0.0)
                    })
                    .collect();
                for axis in 0..2 {
                    let mut raw = vec![0.0; n];
                    path.raw_residual(axis, &field[axis], &mut raw);
                    let (mut u, mut p) = (0.0, sigma2[0]);
                    let mut innovations = Vec::with_capacity(n);
                    for r in 0..n {
                        if r > 0 {
                            let s = (centers[r][0] - centers[r - 1][0])
                                .hypot(centers[r][1] - centers[r - 1][1]);
                            let phi = (-s / distance[r]).exp();
                            let a = if sigma2[r - 1] > 0.0 {
                                phi * (sigma2[r] / sigma2[r - 1]).sqrt()
                            } else {
                                0.0
                            };
                            u *= a;
                            p = a * a * p + (1.0 - phi * phi) * sigma2[r];
                        }
                        let s = p + path.floor_variance(axis, r);
                        let nu = raw[r] - u;
                        innovations.push(nu / s.sqrt());
                        let k = p / s;
                        u += k * nu;
                        p *= 1.0 - k;
                    }
                    for pair in innovations.windows(2) {
                        num += pair[0] * pair[1];
                    }
                    let ss: f64 = innovations.iter().map(|v| v * v).sum();
                    den += ss;
                    chi2 += ss;
                    rows += n;
                }
            }
            println!(
                "CAMPBELL case={:?} map={label} unseen={unseen} admissible_area={area:.0} rows={rows} chi2_per_row={:.3e} lag1={:.3}",
                case.name,
                chi2 / rows as f64,
                num / den
            );
        }
    }
}

/// P13 falsifier: a stationary Matern-3/2 model-error process in arc length along each path
/// (scale `s`, length `l`, one pair per map, estimated by maximizing the Kalman marginal
/// likelihood on a grid) on top of the measurement floor. For the true map and the true map
/// minus `DROP` central planets, prints the estimate and the innovations' chi2/row and lag-1
/// autocorrelation. Pass: chi2/row near 1 and lag-1 below about 0.3 for both maps.
#[test]
#[ignore]
fn diagnose_matern_model_error() {
    let drop: usize = std::env::var("DROP")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    let world = WorldParameters::default();
    // Innovations of one axis of one path: (whitened innovations, log-likelihood).
    // `shape[t]` multiplies the scale `s` and the length `l` at row `t` (1 when stationary).
    let filter = |raw: &[f64],
                  floor: &[f64],
                  step: &[f64],
                  shape: &[(f64, f64)],
                  s: f64,
                  l: f64|
     -> (Vec<f64>, f64) {
        let (mut m, mut p) = ([0.0, 0.0], [[0.0, 0.0], [0.0, 0.0]]);
        let mut out = Vec::with_capacity(raw.len());
        let mut loglik = 0.0;
        for t in 0..raw.len() {
            let (ss, ll) = (s * shape[t].0, l * shape[t].1);
            let lam = 3f64.sqrt() / ll;
            let pinf = [ss * ss, lam * lam * ss * ss];
            if t == 0 {
                p = [[pinf[0], 0.0], [0.0, pinf[1]]];
            } else if ss > 0.0 {
                // Rescale the state to this row's scale, then step the stationary process.
                let ratio = if shape[t - 1].0 > 0.0 {
                    shape[t].0 / shape[t - 1].0
                } else {
                    0.0
                };
                m = [m[0] * ratio, m[1] * ratio];
                for row in p.iter_mut() {
                    for v in row.iter_mut() {
                        *v *= ratio * ratio;
                    }
                }
                let d = step[t];
                let e = (-lam * d).exp();
                let f = [
                    [e * (1.0 + lam * d), e * d],
                    [-e * lam * lam * d, e * (1.0 - lam * d)],
                ];
                let mm = [
                    f[0][0] * m[0] + f[0][1] * m[1],
                    f[1][0] * m[0] + f[1][1] * m[1],
                ];
                // P = F P F' + (Pinf - F Pinf F').
                let mul = |a: [[f64; 2]; 2], b: [[f64; 2]; 2]| {
                    let mut c = [[0.0; 2]; 2];
                    for i in 0..2 {
                        for j in 0..2 {
                            c[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j];
                        }
                    }
                    c
                };
                let ft = [[f[0][0], f[1][0]], [f[0][1], f[1][1]]];
                let fpf = mul(mul(f, p), ft);
                let pi = [[pinf[0], 0.0], [0.0, pinf[1]]];
                let fif = mul(mul(f, pi), ft);
                for i in 0..2 {
                    for j in 0..2 {
                        p[i][j] = fpf[i][j] + pi[i][j] - fif[i][j];
                    }
                }
                m = mm;
            }
            let sv = p[0][0] + floor[t];
            let nu = raw[t] - m[0];
            out.push(nu / sv.sqrt());
            loglik += -0.5 * (nu * nu / sv + sv.ln());
            let k = [p[0][0] / sv, p[1][0] / sv];
            m = [m[0] + k[0] * nu, m[1] + k[1] * nu];
            let p00 = p[0][0];
            let p01 = p[0][1];
            p = [
                [p[0][0] - k[0] * p00, p[0][1] - k[0] * p01],
                [p[1][0] - k[1] * p00, p[1][1] - k[1] * p01],
            ];
        }
        (out, loglik)
    };
    for case in cases() {
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
        let fit = Fit::new(&case.shots, world, &cancelled, false, Vec::new());
        for (label, theta) in [("true", &truth), ("missing", &partial)] {
            let campbell = std::env::var("SHAPE").is_ok_and(|v| v == "campbell");
            // Admissible centres of an unseen planet on a 10-unit raster (for the Campbell shape:
            // scale 1 / d_U, length d_U, d_U the distance to the admissible region).
            let mut admissible: Vec<[f64; 2]> = Vec::new();
            if campbell {
                let found: Vec<Planet> = theta.iter().map(|t| planet_at(*t)).collect();
                let ticks: Vec<[f64; 2]> = case
                    .shots
                    .iter()
                    .filter_map(|s| s.path.as_ref())
                    .flat_map(|p| p.ticks.iter().copied())
                    .collect();
                let (w, h) = (world.battlefield_width, world.battlefield_height);
                let mut y = 25.0;
                while y < h - 20.0 {
                    let mut x = 25.0;
                    while x < w - 20.0 {
                        if found
                            .iter()
                            .all(|p| (p.x - x).hypot(p.y - y) > p.radius + 20.0)
                            && ticks.iter().all(|t| (t[0] - x).hypot(t[1] - y) > 20.0)
                        {
                            admissible.push([x, y]);
                        }
                        x += 10.0;
                    }
                    y += 10.0;
                }
            }
            // (raw residual, floor variance, step, shape) per path and axis.
            let mut series: Vec<(Vec<f64>, Vec<f64>, Vec<f64>, Vec<(f64, f64)>)> = Vec::new();
            for (i, shot) in case.shots.iter().enumerate() {
                let Some(path) = &shot.path else { continue };
                let n = path.row_count();
                let field = fit.field(i, path, theta);
                let step: Vec<f64> = (0..n)
                    .map(|r| {
                        if r == 0 {
                            0.0
                        } else {
                            let (a, b) = (path.center(r - 1), path.center(r));
                            (a[0] - b[0]).hypot(a[1] - b[1])
                        }
                    })
                    .collect();
                let shape: Vec<(f64, f64)> = (0..n)
                    .map(|r| {
                        if !campbell {
                            return (1.0, 1.0);
                        }
                        let c = path.center(r);
                        let d = admissible
                            .iter()
                            .map(|a| (a[0] - c[0]).hypot(a[1] - c[1]))
                            .fold(f64::INFINITY, f64::min)
                            .max(20.0);
                        (100.0 / d, d / 100.0)
                    })
                    .collect();
                for axis in 0..2 {
                    let mut raw = vec![0.0; n];
                    path.raw_residual(axis, &field[axis], &mut raw);
                    let floor = (0..n).map(|r| path.floor_variance(axis, r)).collect();
                    series.push((raw, floor, step.clone(), shape.clone()));
                }
            }
            let total = |s: f64, l: f64| -> f64 {
                series
                    .iter()
                    .map(|(raw, floor, step, shape)| filter(raw, floor, step, shape, s, l).1)
                    .sum()
            };
            let mut best = (total(0.0, 1.0), 0.0, 1.0);
            for si in 0..33 {
                let s = 1e-6 * 10f64.powf(si as f64 / 4.0);
                for li in 0..13 {
                    let l = 5.0 * 10f64.powf(li as f64 / 4.0);
                    let v = total(s, l);
                    if v > best.0 {
                        best = (v, s, l);
                    }
                }
            }
            let (_, s, l) = best;
            let (mut chi2, mut rows, mut num, mut den) = (0.0, 0usize, 0.0, 0.0);
            for (raw, floor, step, shape) in &series {
                let (w, _) = filter(raw, floor, step, shape, s, l);
                for pair in w.windows(2) {
                    num += pair[0] * pair[1];
                }
                let ss: f64 = w.iter().map(|v| v * v).sum();
                den += ss;
                chi2 += ss;
                rows += w.len();
            }
            println!(
                "MATERN case={:?} shape={} map={label} s={s:.3e} l={l:.1} rows={rows} chi2_per_row={:.3} lag1={:.3}",
                case.name,
                if campbell { "campbell" } else { "stationary" },
                chi2 / rows as f64,
                num / den
            );
        }
    }
}
