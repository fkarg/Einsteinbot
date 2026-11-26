//! Offline probe (P3): how much accuracy does full-path simulation add to planet estimation?
//!
//! Three likelihoods share one parameterization, optimizer, start set and evidence rule, so only
//! the residual differs:
//!   S    full-path simulation from the launch state (engine-faithful velocity Verlet).
//!   W    windowed field comparison, no dynamics: the observed velocity change over a window of n
//!        ticks is compared with the engine's exact discrete kernel applied to the model field along
//!        the observed path (interpolated between ticks). Rounding noise covariance is exact.
//!   A    abbreviated simulation: from each observed tick state integrate m ticks and compare.
//! Truth is used only to score (and to build synthetic data); it is never a fit input.
//! Run `likelihood_value_probe help` for the experiments. Offline only; no live estimator change.
use einsteinbot::coverage::CELL_SIZE;
use einsteinbot::estimator::Estimator;
use einsteinbot::event_store::EventLog;
use einsteinbot::field_score::score_field;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt, TerminationReason};
use nalgebra::{DMatrix, DVector, Dyn, U1, VecStorage};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

const N: usize = 25;
const DT: f64 = 1.0 / N as f64;
const RADIUS_RANGE: (f64, f64) = (20.0, 40.0);
const PENALTY_SIGMA: f64 = 0.5;
const PRIOR_ODDS: f64 = 20.0;
const LM_PATIENCE: usize = 40;
const MAX_STARTS: usize = 40;
const MIN_TICKS: usize = 8;

type V2 = [f64; 2];

fn world() -> WorldParameters {
    WorldParameters::default()
}

// ---------------------------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------------------------

struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
    fn normal(&mut self) -> f64 {
        let (a, b) = (self.unit().max(1e-300), self.unit());
        (-2.0 * a.ln()).sqrt() * (2.0 * std::f64::consts::PI * b).cos()
    }
    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[(self.next_u64() % items.len() as u64) as usize]
    }
}

fn par_map<T: Sync, R: Send>(items: &[T], threads: usize, f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..threads.min(items.len()).max(1) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= items.len() {
                        break;
                    }
                    let r = f(&items[i]);
                    results.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut v = results.into_inner().unwrap();
    v.sort_by_key(|x| x.0);
    v.into_iter().map(|x| x.1).collect()
}

fn median(v: &mut Vec<f64>) -> f64 {
    quantile(v, 0.5)
}

fn quantile(v: &mut Vec<f64>, q: f64) -> f64 {
    v.retain(|x| !x.is_nan());
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn g(x: f64) -> String {
    if x.is_nan() {
        "-".into()
    } else if x == 0.0 {
        "0".into()
    } else if x.abs() >= 1000.0 || x.abs() < 0.01 {
        format!("{x:.2e}")
    } else {
        format!("{x:.3}")
    }
}

/// Standard deviation of float32 rounding (uniform within +-ulp/2) of a position value.
fn f32_sd(x: f64) -> f64 {
    let a = (x as f32).abs();
    if a == 0.0 || !a.is_finite() {
        return 1e-6;
    }
    let e = a.log2().floor() as i32;
    (2f64.powi(e - 23) / 12f64.sqrt()).max(1e-7)
}

fn acc(map: &PlanetMap, p: V2) -> Option<V2> {
    map.acceleration(Vec2::new(p[0], p[1]))
        .ok()
        .map(|a| [a.x, a.y])
}

/// Engine velocity-Verlet substep (same arithmetic order as `simulation.rs`).
fn step(map: &PlanetMap, pos: &mut V2, vel: &mut V2, a: &mut V2) -> Option<()> {
    let np = [
        pos[0] + vel[0] * DT + a[0] * 0.5 * DT * DT,
        pos[1] + vel[1] * DT + a[1] * 0.5 * DT * DT,
    ];
    let na = acc(map, np)?;
    vel[0] += (a[0] + na[0]) * 0.5 * DT;
    vel[1] += (a[1] + na[1]) * 0.5 * DT;
    *pos = np;
    *a = na;
    Some(())
}

fn launch_velocity(angle_degrees: f64, speed: f64) -> V2 {
    let a = angle_degrees.to_radians();
    [speed * a.cos(), -speed * a.sin()]
}

// ---------------------------------------------------------------------------------------------
// Synthetic flights and paths
// ---------------------------------------------------------------------------------------------

struct Flight {
    sub: Vec<V2>,
    ticks: usize,
    terminated: bool,
}

fn fly(map: &PlanetMap, p0: V2, v0: V2, max_ticks: usize) -> Option<Flight> {
    let w = world();
    let (mut pos, mut vel) = (p0, v0);
    let mut a = acc(map, pos)?;
    let mut sub = vec![pos];
    let mut terminated = false;
    for _ in 0..max_ticks * N {
        step(map, &mut pos, &mut vel, &mut a)?;
        sub.push(pos);
        let hit = map.planets.iter().any(|p| {
            let (dx, dy) = (p.x - pos[0], p.y - pos[1]);
            dx * dx + dy * dy <= p.radius * p.radius
        });
        let out = pos[0] < -w.margin
            || pos[0] > w.battlefield_width + w.margin
            || pos[1] < -w.margin
            || pos[1] > w.battlefield_height + w.margin;
        if hit || out {
            terminated = true;
            break;
        }
    }
    let n_sub = sub.len() - 1;
    let ticks = if terminated {
        (n_sub - 1) / N + 1
    } else {
        n_sub / N + 1
    };
    Some(Flight {
        sub,
        ticks,
        terminated,
    })
}

struct FlightRec {
    fl: Flight,
    angle: f64,
    speed: f64,
    v0: V2,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Data {
    /// Positions rounded to f32 as the protocol delivers them.
    Rounded,
    /// Float64 positions (isolates rounding noise).
    Unrounded,
    /// Rounded observations, but W's field nodes are the TRUE substep positions (isolates
    /// interpolation error; uses truth by design, only in this isolation run).
    TrueSub,
}

#[derive(Clone)]
struct Path {
    ticks: Vec<V2>,
    sd: Vec<V2>,
    v0: V2,
    /// True substep positions through the last regular tick (synthetic only; empty otherwise).
    sub: Vec<V2>,
    obs: ShotObservation,
    speed: f64,
    dmin: f64,
}

fn dmin_of(ticks: &[V2], truth: &PlanetMap) -> f64 {
    ticks
        .iter()
        .flat_map(|t| {
            truth
                .planets
                .iter()
                .map(move |p| (p.x - t[0]).hypot(p.y - t[1]))
        })
        .fold(f64::INFINITY, f64::min)
}

fn make_points(ticks: &[V2], terminal: V2) -> Vec<Point> {
    ticks
        .iter()
        .chain(std::iter::once(&terminal))
        .map(|t| Point {
            x: t[0] as f32,
            y: t[1] as f32,
        })
        .collect()
}

fn path_from_flight(fr: &FlightRec, data: Data, truth: &PlanetMap) -> Path {
    let t = fr.fl.ticks;
    let exact: Vec<V2> = (0..t).map(|s| fr.fl.sub[s * N]).collect();
    let terminal = if fr.fl.terminated {
        *fr.fl.sub.last().unwrap()
    } else {
        *exact.last().unwrap()
    };
    let points = make_points(&exact, terminal);
    let ticks: Vec<V2> = match data {
        Data::Unrounded => exact.clone(),
        _ => points[..t]
            .iter()
            .map(|p| [f64::from(p.x), f64::from(p.y)])
            .collect(),
    };
    let sd = ticks.iter().map(|p| [f32_sd(p[0]), f32_sd(p[1])]).collect();
    Path {
        dmin: dmin_of(&exact, truth),
        sub: fr.fl.sub[..=(t - 1) * N].to_vec(),
        ticks,
        sd,
        v0: fr.v0,
        obs: ShotObservation {
            player_id: 0,
            angle_degrees: fr.angle,
            velocity: fr.speed,
            points,
        },
        speed: fr.speed,
    }
}

/// Adds Gaussian position noise `extra` to ticks 1.. (mis-modelled when `told` is false).
fn perturb(path: &mut Path, extra: f64, told: bool, rng: &mut Rng) {
    for s in 1..path.ticks.len() {
        for a in 0..2 {
            path.ticks[s][a] += extra * rng.normal();
            if told {
                path.sd[s][a] = path.sd[s][a].hypot(extra);
            }
        }
    }
}

fn path_from_observation(obs: &ShotObservation, truth: &PlanetMap) -> Option<Path> {
    let t = obs.points.len().checked_sub(1)?;
    if t < MIN_TICKS {
        return None;
    }
    let ticks: Vec<V2> = obs.points[..t]
        .iter()
        .map(|p| [f64::from(p.x), f64::from(p.y)])
        .collect();
    let sd = ticks.iter().map(|p| [f32_sd(p[0]), f32_sd(p[1])]).collect();
    Some(Path {
        dmin: dmin_of(&ticks, truth),
        sd,
        v0: launch_velocity(obs.angle_degrees, obs.velocity),
        sub: Vec::new(),
        obs: obs.clone(),
        speed: obs.velocity,
        ticks,
    })
}

fn gen_world(rng: &mut Rng, k: usize) -> PlanetMap {
    let w = world();
    let mut planets: Vec<Planet> = Vec::new();
    while planets.len() < k {
        let radius = rng.range(RADIUS_RANGE.0, RADIUS_RANGE.1);
        let x = rng.range(radius, w.battlefield_width - radius);
        let y = rng.range(radius, w.battlefield_height - radius);
        if planets
            .iter()
            .all(|p| (p.x - x).hypot(p.y - y) > p.radius + radius + 20.0)
        {
            planets.push(Planet {
                x,
                y,
                radius,
                mass: 0.1 * radius.powi(3),
            });
        }
    }
    PlanetMap { planets }
}

/// A shot aimed to pass a random planet at straight-line impact parameter `b`, or blind.
fn gen_flight(rng: &mut Rng, truth: &PlanetMap, aimed: bool, target: usize) -> Option<FlightRec> {
    let w = world();
    for _ in 0..200 {
        let p0 = [
            rng.range(5.0, w.battlefield_width - 5.0),
            rng.range(5.0, w.battlefield_height - 5.0),
        ];
        if truth
            .planets
            .iter()
            .any(|p| (p.x - p0[0]).hypot(p.y - p0[1]) < p.radius + 8.0)
        {
            continue;
        }
        let speed = rng.range(2f64.ln(), 50f64.ln()).exp();
        let angle = if aimed && !truth.planets.is_empty() {
            let p = &truth.planets[target % truth.planets.len()];
            let b = rng.pick(&[0.0, 15.0, 30.0, 50.0, 80.0, 120.0, 180.0, 260.0, 400.0])
                * rng.pick(&[-1.0, 1.0]);
            let (dx, dy) = (p.x - p0[0], p.y - p0[1]);
            let d = dx.hypot(dy);
            if d < 100.0 {
                continue;
            }
            let (ax, ay) = (p.x - dy / d * b, p.y + dx / d * b);
            (-(ay - p0[1])).atan2(ax - p0[0]).to_degrees()
        } else {
            rng.range(0.0, 360.0)
        };
        let max_ticks = rng.pick(&[20usize, 60, 180]);
        let v0 = launch_velocity(angle, speed);
        let fl = fly(truth, p0, v0, max_ticks)?;
        if fl.ticks >= MIN_TICKS {
            return Some(FlightRec {
                fl,
                angle,
                speed,
                v0,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Observed-path interpolation and the engine's exact discrete kernel
// ---------------------------------------------------------------------------------------------

/// Per-tick velocity estimate at tick `s`: exact launch velocity at 0, 4th-order central
/// difference inside, 2nd-order central/one-sided at the ends.
fn tangent(ticks: &[V2], v0: V2, s: usize) -> V2 {
    let t = ticks.len();
    let mut out = [0.0; 2];
    for a in 0..2 {
        let p = |i: usize| ticks[i][a];
        out[a] = if s == 0 {
            v0[a]
        } else if s == t - 1 {
            (3.0 * p(s) - 4.0 * p(s - 1) + p(s - 2)) / 2.0
        } else if s == 1 || s + 2 > t - 1 {
            (p(s + 1) - p(s - 1)) / 2.0
        } else {
            (-p(s + 2) + 8.0 * p(s + 1) - 8.0 * p(s - 1) + p(s - 2)) / 12.0
        };
    }
    out
}

/// Variance of the tangent estimate at tick `s` from independent tick noise.
fn tangent_var(sd: &[V2], s: usize) -> V2 {
    let t = sd.len();
    let mut out = [0.0; 2];
    for a in 0..2 {
        let v = |i: usize| sd[i][a] * sd[i][a];
        out[a] = if s == 0 {
            0.0
        } else if s == t - 1 {
            (9.0 * v(s) + 16.0 * v(s - 1) + v(s - 2)) / 4.0
        } else if s == 1 || s + 2 > t - 1 {
            (v(s + 1) + v(s - 1)) / 4.0
        } else {
            (v(s + 2) + 64.0 * v(s + 1) + 64.0 * v(s - 1) + v(s - 2)) / 144.0
        };
    }
    out
}

/// Cubic Hermite interpolation of the observed ticks (4th-order tangents), evaluated at substep
/// multiples of `q`. With `truth_sub`, returns the true substep positions instead.
fn nodes(path: &Path, q: usize, data: Data) -> Vec<V2> {
    let t = path.ticks.len();
    let total = (t - 1) * N;
    (0..=total / q)
        .map(|j| {
            let m = j * q;
            if data == Data::TrueSub {
                return path.sub[m];
            }
            let (s, i) = (m / N, m % N);
            if s == t - 1 {
                return path.ticks[s];
            }
            let u = i as f64 / N as f64;
            let (h00, h10, h01, h11) = (
                2.0 * u.powi(3) - 3.0 * u * u + 1.0,
                u.powi(3) - 2.0 * u * u + u,
                -2.0 * u.powi(3) + 3.0 * u * u,
                u.powi(3) - u * u,
            );
            let (m0, m1) = (
                tangent(&path.ticks, path.v0, s),
                tangent(&path.ticks, path.v0, s + 1),
            );
            let mut out = [0.0; 2];
            for a in 0..2 {
                out[a] =
                    h00 * path.ticks[s][a] + h10 * m0[a] + h01 * path.ticks[s + 1][a] + h11 * m1[a];
            }
            out
        })
        .collect()
}

/// Substep weights `f(m)` such that sum_m f(m) a_m equals the sum of tick-level second differences
/// at ticks `a..=b`. The engine scheme gives x_{m+1}-2x_m+x_{m-1} = dt^2 a_m exactly, and a
/// tick-level second difference is the substep second difference convolved with the triangle
/// (N-|i|). Returned as (first substep index, weights).
fn window_weights(a: usize, b: usize) -> (usize, Vec<f64>) {
    let lo = a * N - (N - 1);
    let hi = b * N + (N - 1);
    let mut f = vec![0.0; hi - lo + 1];
    for s in a..=b {
        for i in 0..(2 * N - 1) {
            let m = s * N + i + 1 - N;
            f[m - lo] += (N - (m.abs_diff(s * N))) as f64 * DT * DT;
        }
    }
    (lo, f)
}

/// Weights of the launch row: p_1 - p_0 - v0 = dt^2 [N a_0 / 2 + sum_{k=1}^{N-1} (N-k) a_k].
fn launch_weights() -> (usize, Vec<f64>) {
    let mut f = vec![0.0; N];
    f[0] = N as f64 / 2.0 * DT * DT;
    for (k, w) in f.iter_mut().enumerate().skip(1) {
        *w = (N - k) as f64 * DT * DT;
    }
    (0, f)
}

/// Fold substep weights onto nodes spaced `q` apart (acceleration linear in between).
fn fold(lo: usize, f: &[f64], q: usize, n_nodes: usize) -> Vec<(u32, f64)> {
    let mut dense = vec![0.0; n_nodes];
    for (i, w) in f.iter().enumerate() {
        let m = lo + i;
        let (j, r) = (m / q, m % q);
        if r == 0 {
            dense[j] += w;
        } else {
            let lam = r as f64 / q as f64;
            dense[j] += w * (1.0 - lam);
            dense[j + 1] += w * lam;
        }
    }
    dense
        .iter()
        .enumerate()
        .filter(|(_, w)| **w != 0.0)
        .map(|(j, w)| (j as u32, *w))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Prepared likelihoods
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Method {
    S,
    /// Window of `n` ticks, field nodes every `q` substeps (q=1 is the exact kernel).
    W {
        n: usize,
        q: usize,
    },
    A {
        m: usize,
    },
}

impl Method {
    fn label(&self) -> String {
        match self {
            Method::S => "S".into(),
            Method::W { n, q: 1 } => format!("W{n}"),
            Method::W { n, q } => format!("W{n}/q{q}"),
            Method::A { m } => format!("A{m}"),
        }
    }
}

struct WPrep {
    nodes: Vec<V2>,
    rows: Vec<Vec<(u32, f64)>>,
    y: [Vec<f64>; 2],
    chol: [Vec<(usize, Vec<f64>)>; 2],
}

struct Win {
    t: usize,
    m: usize,
    p: V2,
    v: V2,
    sd: Vec<V2>,
}

enum Kind {
    S,
    W(WPrep),
    A(Vec<Win>),
}

struct Prep {
    ticks: Vec<V2>,
    sd: Vec<V2>,
    v0: V2,
    kind: Kind,
    dof: usize,
}

fn prepare(method: Method, path: &Path, data: Data) -> Prep {
    let t = path.ticks.len();
    let mut prep = Prep {
        ticks: path.ticks.clone(),
        sd: path.sd.clone(),
        v0: path.v0,
        kind: Kind::S,
        dof: 2 * (t - 1),
    };
    match method {
        Method::S => {}
        Method::W { n, q } => {
            let n = n.min(t - 3).max(1);
            let stride = (n / 2).max(1);
            let nodes_v = nodes(path, q, data);
            let n_nodes = nodes_v.len();
            // Each row: (substep weights, observed value per axis, noise coefficients on ticks).
            let mut rows: Vec<Vec<(u32, f64)>> = Vec::new();
            let mut coefs: Vec<Vec<(usize, f64)>> = Vec::new();
            let mut y = [Vec::new(), Vec::new()];
            let (lo, f) = launch_weights();
            rows.push(fold(lo, &f, q, n_nodes));
            coefs.push(vec![(1, 1.0), (0, -1.0)]);
            for a in 0..2 {
                y[a].push(path.ticks[1][a] - path.ticks[0][a] - path.v0[a]);
            }
            let mut start = 0;
            while start + n + 1 <= t - 1 {
                let (lo, f) = window_weights(start + 1, start + n);
                rows.push(fold(lo, &f, q, n_nodes));
                let mut c = vec![
                    (start, 1.0),
                    (start + 1, -1.0),
                    (start + n, -1.0),
                    (start + n + 1, 1.0),
                ];
                c.sort_by_key(|x| x.0);
                let mut merged: Vec<(usize, f64)> = Vec::new();
                for (i, w) in c {
                    match merged.last_mut() {
                        Some(last) if last.0 == i => last.1 += w,
                        _ => merged.push((i, w)),
                    }
                }
                coefs.push(merged);
                for a in 0..2 {
                    let p = |i: usize| path.ticks[i][a];
                    y[a].push(p(start + n + 1) - p(start + n) - p(start + 1) + p(start));
                }
                start += stride;
            }
            let r = rows.len();
            let mut chol: [Vec<(usize, Vec<f64>)>; 2] = [Vec::new(), Vec::new()];
            for a in 0..2 {
                let mut cov = DMatrix::zeros(r, r);
                for i in 0..r {
                    for j in 0..r {
                        let mut s = 0.0;
                        for (ti, ci) in &coefs[i] {
                            for (tj, cj) in &coefs[j] {
                                if ti == tj {
                                    s += ci * cj * path.sd[*ti][a].powi(2);
                                }
                            }
                        }
                        cov[(i, j)] = s;
                    }
                }
                let l = cov.cholesky().expect("window covariance is SPD").l();
                for i in 0..r {
                    let first = (0..=i).find(|&j| l[(i, j)] != 0.0).unwrap_or(i);
                    chol[a].push((first, (first..=i).map(|j| l[(i, j)]).collect()));
                }
            }
            prep.dof = 2 * r;
            prep.kind = Kind::W(WPrep {
                nodes: nodes_v,
                rows,
                y,
                chol,
            });
        }
        Method::A { m } => {
            let mut wins = Vec::new();
            let mut start = 0;
            let mut dof = 0;
            while start + m <= t - 1 {
                let (v, vvar) = if start == 0 {
                    (path.v0, [0.0; 2])
                } else {
                    (
                        tangent(&path.ticks, path.v0, start),
                        tangent_var(&path.sd, start),
                    )
                };
                let sd = (1..=m)
                    .map(|j| {
                        let mut o = [0.0; 2];
                        for a in 0..2 {
                            o[a] = (path.sd[start + j][a].powi(2)
                                + path.sd[start][a].powi(2)
                                + (j * j) as f64 * vvar[a])
                                .sqrt();
                        }
                        o
                    })
                    .collect();
                wins.push(Win {
                    t: start,
                    m,
                    p: path.ticks[start],
                    v,
                    sd,
                });
                dof += 2 * m;
                start += m;
            }
            prep.dof = dof;
            prep.kind = Kind::A(wins);
        }
    }
    prep
}

impl Prep {
    fn eval(&self, map: &PlanetMap, out: &mut Vec<f64>) -> Option<()> {
        match &self.kind {
            Kind::S => {
                let (mut pos, mut vel) = (self.ticks[0], self.v0);
                let mut a = acc(map, pos)?;
                for s in 1..self.ticks.len() {
                    for _ in 0..N {
                        step(map, &mut pos, &mut vel, &mut a)?;
                    }
                    for k in 0..2 {
                        out.push((self.ticks[s][k] - pos[k]) / self.sd[s][k]);
                    }
                }
            }
            Kind::A(wins) => {
                for w in wins {
                    let (mut pos, mut vel) = (w.p, w.v);
                    let mut a = acc(map, pos)?;
                    for j in 1..=w.m {
                        for _ in 0..N {
                            step(map, &mut pos, &mut vel, &mut a)?;
                        }
                        for k in 0..2 {
                            out.push((self.ticks[w.t + j][k] - pos[k]) / w.sd[j - 1][k]);
                        }
                    }
                }
            }
            Kind::W(w) => {
                let mut accel = Vec::with_capacity(w.nodes.len());
                for p in &w.nodes {
                    accel.push(acc(map, *p)?);
                }
                for k in 0..2 {
                    let rows = w.rows.len();
                    let mut z = vec![0.0; rows];
                    for i in 0..rows {
                        let pred: f64 = w.rows[i]
                            .iter()
                            .map(|(j, wt)| wt * accel[*j as usize][k])
                            .sum();
                        let (first, l) = &w.chol[k][i];
                        let mut s = w.y[k][i] - pred;
                        for (off, lij) in l[..l.len() - 1].iter().enumerate() {
                            s -= lij * z[first + off];
                        }
                        z[i] = s / l[l.len() - 1];
                    }
                    out.extend_from_slice(&z);
                }
            }
        }
        Some(())
    }
}

// ---------------------------------------------------------------------------------------------
// Fitting (identical for every method)
// ---------------------------------------------------------------------------------------------

struct Problem {
    preps: Vec<Prep>,
    evals: AtomicUsize,
}

fn sigmoid(v: f64) -> f64 {
    1.0 / (1.0 + (-v).exp())
}

fn decode(u: &DVector<f64>) -> Vec<[f64; 3]> {
    let w = world();
    (0..u.len() / 3)
        .map(|i| {
            let radius = RADIUS_RANGE.0 + (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(u[3 * i + 2]);
            [
                radius + (w.battlefield_width - 2.0 * radius) * sigmoid(u[3 * i]),
                radius + (w.battlefield_height - 2.0 * radius) * sigmoid(u[3 * i + 1]),
                radius,
            ]
        })
        .collect()
}

fn encode(theta: &[[f64; 3]]) -> DVector<f64> {
    let w = world();
    let logit = |v: f64| {
        let v = v.clamp(1e-6, 1.0 - 1e-6);
        (v / (1.0 - v)).ln()
    };
    let mut u = Vec::new();
    for t in theta {
        let r = t[2];
        u.push(logit((t[0] - r) / (w.battlefield_width - 2.0 * r)));
        u.push(logit((t[1] - r) / (w.battlefield_height - 2.0 * r)));
        u.push(logit(
            (r - RADIUS_RANGE.0) / (RADIUS_RANGE.1 - RADIUS_RANGE.0),
        ));
    }
    DVector::from_vec(u)
}

fn planets_of(theta: &[[f64; 3]]) -> Vec<Planet> {
    theta
        .iter()
        .map(|t| Planet {
            x: t[0],
            y: t[1],
            radius: t[2],
            mass: 0.1 * t[2].powi(3),
        })
        .collect()
}

struct Lm<'a> {
    prob: &'a Problem,
    u: DVector<f64>,
}

impl LeastSquaresProblem<f64, Dyn, Dyn> for Lm<'_> {
    type ParameterStorage = VecStorage<f64, Dyn, U1>;
    type ResidualStorage = VecStorage<f64, Dyn, U1>;
    type JacobianStorage = VecStorage<f64, Dyn, Dyn>;
    fn set_params(&mut self, u: &DVector<f64>) {
        self.u.copy_from(u);
    }
    fn params(&self) -> DVector<f64> {
        self.u.clone()
    }
    fn residuals(&self) -> Option<DVector<f64>> {
        self.prob
            .residuals(&planets_of(&decode(&self.u)))
            .map(DVector::from_vec)
    }
    fn jacobian(&self) -> Option<DMatrix<f64>> {
        let baseline = self.residuals()?;
        let mut jac = DMatrix::zeros(baseline.len(), self.u.len());
        for c in 0..self.u.len() {
            let h = 1e-4 * (1.0 + self.u[c].abs());
            let mut shifted = self.u.clone();
            shifted[c] += h;
            let plus = self
                .prob
                .residuals(&planets_of(&decode(&shifted)))
                .map(DVector::from_vec)?;
            jac.set_column(c, &((plus - &baseline) / h));
        }
        Some(jac)
    }
}

struct KFit {
    theta: Vec<[f64; 3]>,
    chi2: f64,
}

struct Laplace {
    evidence: f64,
    cov: Option<DMatrix<f64>>,
    cov_inflated: Option<DMatrix<f64>>,
}

fn same_mode(a: &[f64; 3], b: &[f64; 3]) -> bool {
    (a[0] - b[0]).hypot(a[1] - b[1]) < 10.0 && (a[2] - b[2]).abs() < 3.0
}

impl Problem {
    fn new(method: Method, paths: &[Path], data: Data) -> Self {
        Self {
            preps: paths.iter().map(|p| prepare(method, p, data)).collect(),
            evals: AtomicUsize::new(0),
        }
    }

    fn dof(&self) -> usize {
        self.preps.iter().map(|p| p.dof).sum()
    }

    fn residuals(&self, planets: &[Planet]) -> Option<Vec<f64>> {
        self.evals.fetch_add(1, Ordering::Relaxed);
        let map = PlanetMap {
            planets: planets.to_vec(),
        };
        let mut out = Vec::new();
        for p in &self.preps {
            p.eval(&map, &mut out)?;
            // A regular sample inside a model disk is impossible (same term as the estimator).
            for tick in &p.ticks[1..] {
                for pl in planets {
                    let d = (tick[0] - pl.x).hypot(tick[1] - pl.y);
                    out.push((pl.radius - d).max(0.0) / PENALTY_SIGMA);
                }
            }
        }
        out.iter().all(|r| r.is_finite()).then_some(out)
    }

    fn chi2(&self, planets: &[Planet]) -> Option<f64> {
        self.residuals(planets)
            .map(|r| r.iter().map(|x| x * x).sum())
    }

    fn refine(&self, start: &[[f64; 3]]) -> Option<KFit> {
        let (lm, report) = LevenbergMarquardt::new()
            .with_patience(LM_PATIENCE)
            .minimize(Lm {
                prob: self,
                u: encode(start),
            });
        if !report.termination.was_successful()
            && !matches!(report.termination, TerminationReason::LostPatience)
        {
            return None;
        }
        let theta = decode(&lm.u);
        let chi2 = self.chi2(&planets_of(&theta))?;
        Some(KFit { theta, chi2 })
    }

    /// Greedy fits K=0..=kmax; index k holds the best fit with k planets (k=0: flat). The same
    /// truth-free start set seeds the new planet at every stage.
    fn fit_sequence(&self, starts: &[[f64; 3]], kmax: usize) -> Vec<KFit> {
        let flat = KFit {
            theta: vec![],
            chi2: self.chi2(&[]).unwrap_or(f64::INFINITY),
        };
        let mut fits = vec![flat];
        for _ in 1..=kmax {
            let base = fits.last().unwrap().theta.clone();
            let best = starts
                .iter()
                .filter_map(|s| {
                    let mut start = base.clone();
                    start.push(*s);
                    self.refine(&start)
                })
                .min_by(|a, b| a.chi2.total_cmp(&b.chi2));
            match best {
                Some(b) => fits.push(b),
                None => break,
            }
        }
        fits
    }

    fn laplace(&self, fit: &KFit) -> Option<Laplace> {
        let k = fit.theta.len();
        let chi2 = fit.chi2;
        if k == 0 {
            return Some(Laplace {
                evidence: -0.5 * chi2,
                cov: None,
                cov_inflated: None,
            });
        }
        let w = world();
        let mut cols = Vec::new();
        for c in 0..3 * k {
            let (mut plus, mut minus) = (fit.theta.clone(), fit.theta.clone());
            plus[c / 3][c % 3] += 0.01;
            minus[c / 3][c % 3] -= 0.01;
            let (rp, rm) = (
                DVector::from_vec(self.residuals(&planets_of(&plus))?),
                DVector::from_vec(self.residuals(&planets_of(&minus))?),
            );
            cols.push((rp - rm) / 0.02);
        }
        let mut info = DMatrix::zeros(3 * k, 3 * k);
        for i in 0..3 * k {
            for j in 0..3 * k {
                info[(i, j)] = cols[i].dot(&cols[j]);
            }
        }
        let prior = [
            w.battlefield_width / 12f64.sqrt(),
            w.battlefield_height / 12f64.sqrt(),
            (RADIUS_RANGE.1 - RADIUS_RANGE.0) / 12f64.sqrt(),
        ];
        let scaled = DMatrix::from_fn(3 * k, 3 * k, |i, j| {
            info[(i, j)] * prior[i % 3] * prior[j % 3] + if i == j { 1.0 } else { 0.0 }
        });
        let occam: f64 = scaled
            .cholesky()
            .map(|c| (0..3 * k).map(|i| c.l()[(i, i)].ln()).sum())
            .unwrap_or(f64::NAN);
        let evidence = -0.5 * chi2 - occam - k as f64 * PRIOR_ODDS.ln();
        let cov = info.clone().cholesky().map(|c| c.inverse());
        let dof = (self.dof() as f64 - 3.0 * k as f64).max(1.0);
        let cov_inflated = cov.as_ref().map(|c| c * (chi2 / dof).max(1.0));
        Some(Laplace {
            evidence,
            cov,
            cov_inflated,
        })
    }
}

/// Truth-free start set shared by all methods: per-path acceleration-peak proposals (as the live
/// estimator does) followed by a coarse grid.
fn make_starts(paths: &[Path]) -> Vec<[f64; 3]> {
    let w = world();
    let mut peaks: Vec<(f64, V2, V2)> = Vec::new();
    for p in paths {
        let t = p.ticks.len();
        let best = (1..t - 1)
            .map(|s| {
                let a = [
                    p.ticks[s + 1][0] - 2.0 * p.ticks[s][0] + p.ticks[s - 1][0],
                    p.ticks[s + 1][1] - 2.0 * p.ticks[s][1] + p.ticks[s - 1][1],
                ];
                (a[0].hypot(a[1]), p.ticks[s], a)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(b) = best.filter(|b| b.0 > 3e-3) {
            peaks.push(b);
        }
    }
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut proposals: Vec<[f64; 3]> = Vec::new();
    for (mag, pos, a) in peaks {
        for r in [20.0, 30.0, 40.0] {
            let d = (0.1 * r * r * r / mag).sqrt();
            proposals.push([pos[0] + a[0] / mag * d, pos[1] + a[1] / mag * d, r]);
        }
    }
    for gx in 0..6 {
        for gy in 0..4 {
            let r = 30.0;
            proposals.push([
                r + (w.battlefield_width - 2.0 * r) * (gx as f64 + 0.5) / 6.0,
                r + (w.battlefield_height - 2.0 * r) * (gy as f64 + 0.5) / 4.0,
                r,
            ]);
        }
    }
    let inb = |t: &[f64; 3]| {
        t[0] >= t[2]
            && t[0] <= w.battlefield_width - t[2]
            && t[1] >= t[2]
            && t[1] <= w.battlefield_height - t[2]
    };
    let mut kept: Vec<[f64; 3]> = Vec::new();
    for p in proposals {
        if inb(&p) && kept.len() < MAX_STARTS && !kept.iter().any(|o| same_mode(o, &p)) {
            kept.push(p);
        }
    }
    kept
}

// ---------------------------------------------------------------------------------------------
// Scoring (truth enters only here)
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Rec {
    method: String,
    tag: String,
    npaths: usize,
    dmin: f64,
    speed: f64,
    len: usize,
    radius: f64,
    k_true: usize,
    k_sel: usize,
    ev: Vec<f64>,
    pairs: usize,
    centre: f64,
    rerr: f64,
    mrel: f64,
    z: Vec<f64>,
    zi: Vec<f64>,
    explored: f64,
    unexplored: f64,
    flat_explored: f64,
    flat_unexplored: f64,
    evals: usize,
    ms: f64,
    search_fail: Option<bool>,
}

fn match_planets(truth: &[Planet], est: &[Planet]) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let (mut used_t, mut used_e) = (vec![false; truth.len()], vec![false; est.len()]);
    loop {
        let mut best: Option<(f64, usize, usize)> = None;
        for (i, t) in truth.iter().enumerate().filter(|(i, _)| !used_t[*i]) {
            for (j, e) in est.iter().enumerate().filter(|(j, _)| !used_e[*j]) {
                let d = (t.x - e.x).hypot(t.y - e.y);
                if best.is_none_or(|b| d < b.0) {
                    best = Some((d, i, j));
                }
            }
        }
        let Some((_, i, j)) = best else { break };
        used_t[i] = true;
        used_e[j] = true;
        pairs.push((i, j));
    }
    pairs
}

fn field_rmse(truth: &PlanetMap, est: &PlanetMap, paths: &[Path]) -> (f64, f64) {
    let obs: Vec<ShotObservation> = paths.iter().map(|p| p.obs.clone()).collect();
    let s = score_field(truth, &MapBelief::new(est.clone(), None), &obs, world());
    let get = |f: Option<&einsteinbot::field_score::FieldStats>| {
        f.and_then(|f| f.vector_rmse).unwrap_or(f64::NAN)
    };
    match s {
        Some(s) => (get(s.explored.as_ref()), get(s.unexplored.as_ref())),
        None => (f64::NAN, f64::NAN),
    }
}

struct Spec<'a> {
    method: Method,
    data: Data,
    tag: &'a str,
    kmax: usize,
    /// Score the fit with this many planets; `None` scores the evidence-selected K.
    score_k: Option<usize>,
}

fn evaluate(spec: &Spec, paths: &[Path], truth: &PlanetMap) -> Rec {
    let start = Instant::now();
    let prob = Problem::new(spec.method, paths, spec.data);
    let starts = make_starts(paths);
    let fits = prob.fit_sequence(&starts, spec.kmax);
    let laps: Vec<Option<Laplace>> = fits.iter().map(|f| prob.laplace(f)).collect();
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let ev: Vec<f64> = laps
        .iter()
        .map(|l| l.as_ref().map_or(f64::NEG_INFINITY, |l| l.evidence))
        .collect();
    let k_sel = (0..ev.len())
        .max_by(|a, b| ev[*a].total_cmp(&ev[*b]))
        .unwrap_or(0);
    let k_score = spec.score_k.unwrap_or(k_sel).min(fits.len() - 1);
    let est = planets_of(&fits[k_score].theta);
    let est_map = PlanetMap {
        planets: est.clone(),
    };
    let pairs = match_planets(&truth.planets, &est);
    let (mut cen, mut rerr, mut mrel) = (vec![], vec![], vec![]);
    let (mut z, mut zi) = (vec![], vec![]);
    for (i, j) in &pairs {
        let (t, e) = (&truth.planets[*i], &est[*j]);
        let d = [e.x - t.x, e.y - t.y, e.radius - t.radius];
        cen.push(d[0].hypot(d[1]));
        rerr.push(d[2].abs());
        mrel.push((e.mass / t.mass - 1.0).abs());
        if let Some(l) = &laps[k_score] {
            for (cov, out) in [(&l.cov, &mut z), (&l.cov_inflated, &mut zi)] {
                if let Some(c) = cov {
                    for a in 0..3 {
                        let var = c[(3 * j + a, 3 * j + a)];
                        if var > 0.0 {
                            out.push(d[a] / var.sqrt());
                        }
                    }
                }
            }
        }
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let (explored, unexplored) = field_rmse(truth, &est_map, paths);
    let (flat_explored, flat_unexplored) = field_rmse(truth, &PlanetMap { planets: vec![] }, paths);
    let search_fail = (spec.score_k.is_some() && !truth.planets.is_empty()).then(|| {
        let k = spec.score_k.unwrap();
        k == truth.planets.len()
            && k < fits.len()
            && prob
                .chi2(&truth.planets)
                .is_some_and(|c| c < fits[k].chi2 * (1.0 - 1e-6) - 1e-6)
    });
    Rec {
        method: spec.method.label(),
        tag: spec.tag.to_owned(),
        npaths: paths.len(),
        dmin: paths.iter().map(|p| p.dmin).fold(f64::INFINITY, f64::min),
        speed: paths.iter().map(|p| p.speed).sum::<f64>() / paths.len() as f64,
        len: paths.iter().map(|p| p.ticks.len()).sum::<usize>() / paths.len(),
        radius: truth.planets.first().map_or(f64::NAN, |p| p.radius),
        k_true: truth.planets.len(),
        k_sel,
        ev,
        pairs: pairs.len(),
        centre: mean(&cen),
        rerr: mean(&rerr),
        mrel: mean(&mrel),
        z,
        zi,
        explored,
        unexplored,
        flat_explored,
        flat_unexplored,
        evals: prob.evals.load(Ordering::Relaxed),
        ms,
        search_fail,
    }
}

/// The existing estimator (public API, its own noise heuristic and starts), K<=1.
fn evaluate_reference(paths: &[Path], truth: &PlanetMap, tag: &str) -> Rec {
    let start = Instant::now();
    let mut estimator = Estimator::default();
    let events: Vec<ServerEvent> = paths
        .iter()
        .map(|p| ServerEvent::ShotFin {
            player_id: 0,
            angle: p.obs.angle_degrees,
            velocity: p.obs.velocity,
            points: p.obs.points.clone(),
        })
        .collect();
    let belief = estimator.observe_batch(&events, &AtomicBool::new(false));
    let ms = start.elapsed().as_secs_f64() * 1e3;
    let est = belief.map_or_else(Vec::new, |b| b.map.planets.clone());
    let belief_cov = estimator
        .belief()
        .and_then(|b| b.parameter_covariance.clone());
    let pairs = match_planets(&truth.planets, &est);
    let mut z = vec![];
    let (mut cen, mut rerr, mut mrel) = (vec![], vec![], vec![]);
    for (i, j) in &pairs {
        let (t, e) = (&truth.planets[*i], &est[*j]);
        cen.push((e.x - t.x).hypot(e.y - t.y));
        rerr.push((e.radius - t.radius).abs());
        mrel.push((e.mass / t.mass - 1.0).abs());
        if let (Some(c), 0) = (&belief_cov, *j) {
            z.push((e.x - t.x) / c[(0, 0)].sqrt());
            z.push((e.y - t.y) / c[(1, 1)].sqrt());
            z.push((e.mass - t.mass) / c[(2, 2)].sqrt());
        }
    }
    let mean = |v: &[f64]| {
        if v.is_empty() {
            f64::NAN
        } else {
            v.iter().sum::<f64>() / v.len() as f64
        }
    };
    let (explored, unexplored) = field_rmse(
        truth,
        &PlanetMap {
            planets: est.clone(),
        },
        paths,
    );
    let (fe, fu) = field_rmse(truth, &PlanetMap { planets: vec![] }, paths);
    Rec {
        method: "Sref".into(),
        tag: tag.to_owned(),
        npaths: paths.len(),
        dmin: paths.iter().map(|p| p.dmin).fold(f64::INFINITY, f64::min),
        speed: paths.iter().map(|p| p.speed).sum::<f64>() / paths.len() as f64,
        len: paths.iter().map(|p| p.ticks.len()).sum::<usize>() / paths.len(),
        radius: truth.planets.first().map_or(f64::NAN, |p| p.radius),
        k_true: truth.planets.len(),
        k_sel: est.len(),
        ev: vec![],
        pairs: pairs.len(),
        centre: mean(&cen),
        rerr: mean(&rerr),
        mrel: mean(&mrel),
        z,
        zi: vec![],
        explored,
        unexplored,
        flat_explored: fe,
        flat_unexplored: fu,
        evals: 0,
        ms,
        search_fail: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------------

fn table(title: &str, recs: &[Rec], key: &dyn Fn(&Rec) -> String) {
    println!("\n### {title}\n");
    println!(
        "| group | method | n | recov% | centre med | centre p90 | dR med | dM/M med | expl RMSE med (flat) | unexpl RMSE med (flat) | z med|z| / <2 (infl <2) | search-fail% | evals med | ms med |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|");
    let mut groups: Vec<(String, String)> = Vec::new();
    for r in recs {
        let k = (key(r), r.method.clone());
        if !groups.contains(&k) {
            groups.push(k);
        }
    }
    groups.sort_by(|a, b| a.0.cmp(&b.0));
    for (grp, m) in groups {
        let rs: Vec<&Rec> = recs
            .iter()
            .filter(|r| key(r) == grp && r.method == m)
            .collect();
        let col = |f: &dyn Fn(&Rec) -> f64| -> Vec<f64> { rs.iter().map(|r| f(r)).collect() };
        let recov = rs
            .iter()
            .filter(|r| r.pairs == r.k_true && r.pairs > 0 && r.centre < 10.0)
            .count() as f64
            / rs.len() as f64
            * 100.0;
        let zs: Vec<f64> = rs
            .iter()
            .flat_map(|r| r.z.iter().map(|z| z.abs()))
            .collect();
        let zis: Vec<f64> = rs
            .iter()
            .flat_map(|r| r.zi.iter().map(|z| z.abs()))
            .collect();
        let cov2 = |v: &[f64]| {
            if v.is_empty() {
                f64::NAN
            } else {
                v.iter().filter(|z| **z < 2.0).count() as f64 / v.len() as f64 * 100.0
            }
        };
        let fails: Vec<&Rec> = rs
            .iter()
            .filter(|r| r.search_fail.is_some())
            .cloned()
            .collect();
        let fail = if fails.is_empty() {
            f64::NAN
        } else {
            fails.iter().filter(|r| r.search_fail == Some(true)).count() as f64 / fails.len() as f64
                * 100.0
        };
        println!(
            "| {grp} | {m} | {} | {:.0} | {} | {} | {} | {} | {} ({}) | {} ({}) | {} / {:.0}% ({:.0}%) | {} | {} | {} |",
            rs.len(),
            recov,
            g(median(&mut col(&|r| r.centre))),
            g(quantile(&mut col(&|r| r.centre), 0.9)),
            g(median(&mut col(&|r| r.rerr))),
            g(median(&mut col(&|r| r.mrel))),
            g(median(&mut col(&|r| r.explored))),
            g(median(&mut col(&|r| r.flat_explored))),
            g(median(&mut col(&|r| r.unexplored))),
            g(median(&mut col(&|r| r.flat_unexplored))),
            g(median(&mut zs.clone())),
            cov2(&zs),
            cov2(&zis),
            g(fail),
            g(median(&mut col(&|r| r.evals as f64))),
            g(median(&mut col(&|r| r.ms))),
        );
    }
}

fn dist_band(r: &Rec) -> String {
    match r.dmin {
        d if d < 40.0 => "a near <40".into(),
        d if d <= 150.0 => "b mid 40-150".into(),
        d if d <= 400.0 => "c far 150-400".into(),
        _ => "d very far >400".into(),
    }
}
fn speed_band(r: &Rec) -> String {
    match r.speed {
        s if s < 10.0 => "a speed <10".into(),
        s if s < 25.0 => "b speed 10-25".into(),
        _ => "c speed >25".into(),
    }
}
fn len_band(r: &Rec) -> String {
    match r.len {
        l if l < 40 => "a len <40".into(),
        l if l <= 100 => "b len 40-100".into(),
        _ => "c len >100".into(),
    }
}
fn radius_band(r: &Rec) -> String {
    match r.radius {
        x if x < 27.0 => "a r 20-27".into(),
        x if x < 34.0 => "b r 27-34".into(),
        _ => "c r 34-40".into(),
    }
}

fn methods_all() -> Vec<Method> {
    let mut v = vec![Method::S];
    for n in [1, 2, 4, 8, 16, 32] {
        v.push(Method::W { n, q: 1 });
    }
    for q in [5, 25] {
        v.push(Method::W { n: 8, q });
    }
    for m in [1, 2, 4, 8] {
        v.push(Method::A { m });
    }
    v
}

// ---------------------------------------------------------------------------------------------
// Experiments
// ---------------------------------------------------------------------------------------------

struct Corpus {
    world: usize,
    truth: PlanetMap,
    flights: Vec<FlightRec>,
}

fn corpus(seed: u64, worlds: usize, k: usize, per_world: usize, aimed_every: usize) -> Vec<Corpus> {
    let mut rng = Rng::new(seed);
    (0..worlds)
        .map(|w| {
            let truth = gen_world(&mut rng, k);
            let flights = (0..per_world)
                .filter_map(|i| gen_flight(&mut rng, &truth, i % aimed_every != 0 || k == 0, i))
                .collect();
            Corpus {
                world: w,
                truth,
                flights,
            }
        })
        .collect()
}

/// Single-path fits: K known (=1), evidence for K=0/1 recorded.
fn single_path_jobs<'a>(c: &'a [Corpus]) -> Vec<(&'a Corpus, usize)> {
    c.iter()
        .flat_map(|w| (0..w.flights.len()).map(move |i| (w, i)))
        .collect()
}

fn run_single(
    corp: &[Corpus],
    methods: &[Method],
    data: Data,
    tag: &str,
    threads: usize,
    reference: bool,
    kmax: usize,
    score_known: bool,
) -> Vec<Rec> {
    let jobs: Vec<((&Corpus, usize), Option<Method>)> = single_path_jobs(corp)
        .into_iter()
        .flat_map(|j| {
            methods
                .iter()
                .map(move |m| (j, Some(*m)))
                .chain(reference.then_some((j, None)))
        })
        .collect();
    par_map(&jobs, threads, |((w, i), m)| {
        let path = path_from_flight(&w.flights[*i], data, &w.truth);
        match m {
            Some(m) => evaluate(
                &Spec {
                    method: *m,
                    data,
                    tag,
                    kmax,
                    score_k: score_known.then_some(w.truth.planets.len()),
                },
                std::slice::from_ref(&path),
                &w.truth,
            ),
            None => evaluate_reference(std::slice::from_ref(&path), &w.truth, tag),
        }
    })
}

fn false_planet_table(title: &str, recs: &[Rec]) {
    println!("\n### {title}\n");
    println!("| variant | method | fits | planet admitted % |");
    println!("|---|---|---:|---:|");
    let mut keys: Vec<(String, String)> = recs
        .iter()
        .map(|r| (r.tag.clone(), r.method.clone()))
        .collect();
    keys.dedup();
    keys.sort();
    keys.dedup();
    for (tag, m) in keys {
        let rs: Vec<&Rec> = recs
            .iter()
            .filter(|r| r.tag == tag && r.method == m)
            .collect();
        let admitted = rs.iter().filter(|r| r.k_sel >= 1).count();
        println!(
            "| {tag} | {m} | {} | {:.1} |",
            rs.len(),
            admitted as f64 / rs.len() as f64 * 100.0
        );
    }
}

fn exp_e1(threads: usize, quick: bool) {
    let t0 = Instant::now();
    let worlds = if quick { 4 } else { 32 };
    let corp = corpus(1001, worlds, 1, 8, 4);
    let mut methods = methods_all();
    methods.retain(|_| true);
    let recs = run_single(
        &corp,
        &methods,
        Data::Rounded,
        "f32",
        threads,
        true,
        1,
        true,
    );
    println!(
        "\n## E1: K=1, single-path fits, f32 data ({} worlds x <=8 paths, K known=1; seed 1001)",
        worlds
    );
    table("all", &recs, &|_| "all".into());
    table(
        "by min distance of path to planet centre",
        &recs,
        &dist_band,
    );
    table("by shot speed", &recs, &speed_band);
    table("by path length (ticks)", &recs, &len_band);
    table("by planet radius", &recs, &radius_band);
    // Missed detection: K=1 worlds, evidence-selected K.
    let mut miss: Vec<(String, usize, usize)> = Vec::new();
    for r in recs.iter().filter(|r| !r.ev.is_empty()) {
        let key = format!("{} / {}", r.method, dist_band(r));
        match miss.iter_mut().find(|m| m.0 == key) {
            Some(m) => {
                m.1 += 1;
                m.2 += usize::from(r.k_sel >= 1);
            }
            None => miss.push((key, 1, usize::from(r.k_sel >= 1))),
        }
    }
    miss.sort();
    println!(
        "\n### E1 detection: planet admitted by evidence (K=1 truth), % of single-path fits\n"
    );
    println!("| method / distance band | fits | admitted % |\n|---|---:|---:|");
    for (k, n, a) in miss {
        println!("| {k} | {n} | {:.0} |", a as f64 / n as f64 * 100.0);
    }
    eprintln!("[e1 {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_isolation(threads: usize, quick: bool) {
    let t0 = Instant::now();
    let worlds = if quick { 3 } else { 24 };
    let corp = corpus(2002, worlds, 1, 6, 3);
    let ms = [
        Method::S,
        Method::W { n: 1, q: 1 },
        Method::W { n: 4, q: 1 },
        Method::W { n: 16, q: 1 },
        Method::A { m: 1 },
        Method::A { m: 4 },
    ];
    let mut all = Vec::new();
    for (data, tag) in [
        (Data::Rounded, "f32"),
        (Data::Unrounded, "f64"),
        (Data::TrueSub, "f32+true-substeps"),
    ] {
        let list: Vec<Method> = ms
            .iter()
            .copied()
            .filter(|m| data != Data::TrueSub || matches!(m, Method::W { .. }))
            .collect();
        all.extend(run_single(&corp, &list, data, tag, threads, false, 1, true));
    }
    println!(
        "\n## E2: isolation runs (K=1 known, {} worlds x <=6 paths, single-path fits; seed 2002)",
        worlds
    );
    table("data variant", &all, &|r| r.tag.clone());
    table(
        "data variant, near passes only",
        &all.iter()
            .filter(|r| r.dmin < 40.0)
            .cloned()
            .collect::<Vec<_>>(),
        &|r| r.tag.clone(),
    );
    eprintln!("[isolation {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_flat(threads: usize, quick: bool) {
    let t0 = Instant::now();
    let worlds = if quick { 3 } else { 32 };
    let corp = corpus(3003, worlds, 0, 4, 1);
    let ms = [
        Method::S,
        Method::W { n: 1, q: 1 },
        Method::W { n: 4, q: 1 },
        Method::W { n: 16, q: 1 },
        Method::W { n: 32, q: 1 },
        Method::A { m: 1 },
        Method::A { m: 4 },
        Method::A { m: 8 },
    ];
    let mut all = run_single(
        &corp,
        &ms,
        Data::Rounded,
        "flat f32",
        threads,
        true,
        1,
        false,
    );
    for (extra, told) in [(0.01, true), (0.01, false), (0.1, true), (0.1, false)] {
        let tag = format!(
            "flat f32 + N(0,{extra}) {}",
            if told { "told" } else { "untold" }
        );
        let jobs: Vec<((&Corpus, usize), Method)> = single_path_jobs(&corp)
            .into_iter()
            .flat_map(|j| ms.iter().map(move |m| (j, *m)))
            .collect();
        all.extend(par_map(&jobs, threads, |((w, i), m)| {
            let mut path = path_from_flight(&w.flights[*i], Data::Rounded, &w.truth);
            let mut rng = Rng::new(7000 + (w.world * 31 + *i) as u64);
            perturb(&mut path, extra, told, &mut rng);
            evaluate(
                &Spec {
                    method: *m,
                    data: Data::Rounded,
                    tag: &tag,
                    kmax: 1,
                    score_k: None,
                },
                std::slice::from_ref(&path),
                &w.truth,
            )
        }));
    }
    println!(
        "\n## E3: flat worlds, K chosen by evidence (K=0 vs 1), single-path fits ({} worlds x 4 paths; seed 3003)",
        worlds
    );
    false_planet_table("false-planet rate", &all);
    eprintln!("[flat {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_paths(threads: usize, quick: bool) {
    let t0 = Instant::now();
    let worlds = if quick { 2 } else { 16 };
    let corp = corpus(4004, worlds, 1, 8, 2);
    let ms = [
        Method::S,
        Method::W { n: 4, q: 1 },
        Method::W { n: 16, q: 1 },
        Method::A { m: 2 },
        Method::A { m: 8 },
    ];
    let counts = [1usize, 2, 3, 4, 6, 8];
    let jobs: Vec<(&Corpus, Method, usize)> = corp
        .iter()
        .flat_map(|w| {
            ms.iter()
                .flat_map(move |m| counts.into_iter().map(move |n| (w, *m, n)))
        })
        .collect();
    let recs = par_map(&jobs, threads, |(w, m, n)| {
        let paths: Vec<Path> = w.flights[..(*n).min(w.flights.len())]
            .iter()
            .map(|f| path_from_flight(f, Data::Rounded, &w.truth))
            .collect();
        evaluate(
            &Spec {
                method: *m,
                data: Data::Rounded,
                tag: "paths",
                kmax: 1,
                score_k: Some(1),
            },
            &paths,
            &w.truth,
        )
    });
    println!(
        "\n## E4: paths needed (K=1 known, joint fit of the first n paths, alternating aimed/blind; {} worlds; seed 4004)",
        worlds
    );
    table("by number of paths", &recs, &|r| {
        format!("{:02} paths", r.npaths)
    });
    eprintln!("[paths {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_multi(threads: usize, quick: bool) {
    let t0 = Instant::now();
    let worlds = if quick { 2 } else { 12 };
    let ms = [
        Method::S,
        Method::W { n: 8, q: 1 },
        Method::W { n: 16, q: 1 },
        Method::A { m: 4 },
    ];
    for k in [2usize, 3] {
        let corp = corpus(5000 + k as u64, worlds, k, 8, 1);
        let jobs: Vec<(&Corpus, Method)> = corp
            .iter()
            .flat_map(|w| ms.iter().map(move |m| (w, *m)))
            .collect();
        let recs = par_map(&jobs, threads, |(w, m)| {
            let paths: Vec<Path> = w
                .flights
                .iter()
                .map(|f| path_from_flight(f, Data::Rounded, &w.truth))
                .collect();
            evaluate(
                &Spec {
                    method: *m,
                    data: Data::Rounded,
                    tag: "multi",
                    kmax: k,
                    score_k: Some(k),
                },
                &paths,
                &w.truth,
            )
        });
        println!(
            "\n## E5: K={k} known, joint fit of 8 aimed paths ({worlds} worlds; seed {})",
            5000 + k
        );
        table(&format!("K={k}"), &recs, &|r| format!("K={}", r.k_true));
        // pairs recovered
        println!(
            "\nPlanets matched within 10 units (of {} per world x {} worlds):",
            k, worlds
        );
        for m in &ms {
            let rs: Vec<&Rec> = recs.iter().filter(|r| r.method == m.label()).collect();
            let ok: usize = rs
                .iter()
                .map(|r| usize::from(r.pairs == k && r.centre < 10.0))
                .sum();
            println!(
                "- {}: {} of {} worlds fully recovered (mean-of-pairs centre error < 10)",
                m.label(),
                ok,
                rs.len()
            );
        }
    }
    eprintln!("[multi {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_captures(threads: usize) {
    let t0 = Instant::now();
    let dbs: [(&str, &[i64]); 2] = [
        ("learning-scenarios.sqlite3", &[2, 6, 10, 12, 18]),
        ("einsteinbot.sqlite3", &[44, 49]),
    ];
    let ms = [
        Method::S,
        Method::W { n: 4, q: 1 },
        Method::W { n: 16, q: 1 },
        Method::A { m: 2 },
        Method::A { m: 8 },
    ];
    println!(
        "\n## E6: disclosed-truth captures (first <=8 paths of the epoch with most paths; evidence-selected K up to 4)"
    );
    println!(
        "\n| capture | truth K | paths | method | K sel | explored RMSE (flat) | unexplored RMSE (flat) | ms |\n|---|---:|---:|---|---:|---|---|---:|"
    );
    for (db, ids) in dbs {
        if !std::path::Path::new(db).is_file() {
            println!("| {db} | unreadable |||||||");
            continue;
        }
        for id in ids {
            let Ok(events) = EventLog::read_capture(db, *id) else {
                continue;
            };
            let mut epochs: Vec<(PlanetMap, Vec<ShotObservation>)> = Vec::new();
            for row in events {
                if let RecordedEvent::Inbound(ev) = row.event {
                    match &ev {
                        ServerEvent::Planets { planets } if !planets.is_empty() => {
                            epochs.push((
                                PlanetMap {
                                    planets: planets.clone(),
                                },
                                vec![],
                            ));
                        }
                        ServerEvent::ShotFin { .. } => {
                            if let (Some(e), Ok(obs)) =
                                (epochs.last_mut(), ShotObservation::try_from(&ev))
                            {
                                if !e.1.contains(&obs) {
                                    e.1.push(obs);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            let Some((truth, obs)) = epochs.into_iter().max_by_key(|e| e.1.len()) else {
                continue;
            };
            let paths: Vec<Path> = obs
                .iter()
                .filter_map(|o| path_from_observation(o, &truth))
                .take(8)
                .collect();
            if paths.is_empty() {
                continue;
            }
            let recs = par_map(&ms, threads, |m| {
                evaluate(
                    &Spec {
                        method: *m,
                        data: Data::Rounded,
                        tag: "capture",
                        kmax: 4,
                        score_k: None,
                    },
                    &paths,
                    &truth,
                )
            });
            let mut all = recs;
            all.push(evaluate_reference(&paths, &truth, "capture"));
            for r in all {
                println!(
                    "| {db} #{id} | {} | {} | {} | {} | {} ({}) | {} ({}) | {:.0} |",
                    truth.planets.len(),
                    paths.len(),
                    r.method,
                    r.k_sel,
                    g(r.explored),
                    g(r.flat_explored),
                    g(r.unexplored),
                    g(r.flat_unexplored),
                    r.ms
                );
            }
        }
    }
    eprintln!("[captures {:.0}s]", t0.elapsed().as_secs_f64());
}

fn exp_timing() {
    println!("\n## E7: wall time (single thread; build profile as run)\n");
    let mut rng = Rng::new(9009);
    let truth = gen_world(&mut rng, 1);
    let mut fr = None;
    while fr.is_none() {
        fr = gen_flight(&mut rng, &truth, true, 0)
            .filter(|f| f.fl.ticks >= 100 && f.fl.ticks <= 120);
    }
    let fr = fr.unwrap();
    let path = path_from_flight(&fr, Data::Rounded, &truth);
    println!(
        "Path: {} ticks, speed {:.1}, min centre distance {:.0}.\n",
        path.ticks.len(),
        path.speed,
        path.dmin
    );
    println!(
        "| method | per-eval K=1 (us) | per-eval K=3 (us) | evals per K=1 fit | K=1 fit ms (all starts) | prepare ms |\n|---|---:|---:|---:|---:|---:|"
    );
    let three = PlanetMap {
        planets: vec![
            truth.planets[0].clone(),
            Planet {
                x: 400.0,
                y: 300.0,
                radius: 25.0,
                mass: 0.1 * 25f64.powi(3),
            },
            Planet {
                x: 1400.0,
                y: 700.0,
                radius: 35.0,
                mass: 0.1 * 35f64.powi(3),
            },
        ],
    };
    for m in methods_all() {
        let t = Instant::now();
        let prob = Problem::new(m, std::slice::from_ref(&path), Data::Rounded);
        let prep_ms = t.elapsed().as_secs_f64() * 1e3;
        let time_eval = |planets: &[Planet]| {
            let reps = 200;
            let t = Instant::now();
            for _ in 0..reps {
                std::hint::black_box(prob.residuals(planets));
            }
            t.elapsed().as_secs_f64() * 1e6 / reps as f64
        };
        let e1 = time_eval(&truth.planets);
        let e3 = time_eval(&three.planets);
        prob.evals.store(0, Ordering::Relaxed);
        let starts = make_starts(std::slice::from_ref(&path));
        let t = Instant::now();
        let _ = prob.fit_sequence(&starts, 1);
        let fit_ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "| {} | {:.1} | {:.1} | {} | {:.0} | {:.1} |",
            m.label(),
            e1,
            e3,
            prob.evals.load(Ordering::Relaxed),
            fit_ms,
            prep_ms
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Numerical self-check of the kernel (also run as a unit test)
// ---------------------------------------------------------------------------------------------

struct Check {
    kernel_truth_max: f64,
    kernel_interp_max: f64,
    sim_vs_library_max: f64,
    sim_truth_max: f64,
}

fn selfcheck() -> Check {
    let truth = PlanetMap {
        planets: vec![
            Planet {
                x: 700.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            },
            Planet {
                x: 1100.0,
                y: 300.0,
                radius: 22.0,
                mass: 1064.8,
            },
        ],
    };
    let mut rng = Rng::new(42);
    let fr = loop {
        if let Some(f) = gen_flight(&mut rng, &truth, true, 0).filter(|f| f.fl.ticks >= 30) {
            break f;
        }
    };
    let path = path_from_flight(&fr, Data::Unrounded, &truth);
    let max_abs = |prob: &Problem| {
        let r = prob.residuals(&truth.planets).unwrap();
        // Skip the interior-penalty entries (zero here); report the largest whitened residual.
        r.iter().fold(0.0f64, |a, b| a.max(b.abs()))
    };
    let mut kernel_truth_max = 0.0f64;
    let mut kernel_interp_max = 0.0f64;
    for n in [1, 2, 8, 32] {
        let m = Method::W { n, q: 1 };
        kernel_truth_max = kernel_truth_max.max(max_abs(&Problem::new(
            m,
            std::slice::from_ref(&path),
            Data::TrueSub,
        )));
        kernel_interp_max = kernel_interp_max.max(max_abs(&Problem::new(
            m,
            std::slice::from_ref(&path),
            Data::Unrounded,
        )));
    }
    let sim_truth_max = max_abs(&Problem::new(
        Method::S,
        std::slice::from_ref(&path),
        Data::Unrounded,
    ));
    // My integrator against the library's, from the same f32 launch point.
    let rounded = path_from_flight(&fr, Data::Rounded, &truth);
    let lib = einsteinbot::simulation::simulate_sampled_positions(
        &truth,
        &rounded.obs,
        world(),
        rounded.ticks.len(),
    )
    .unwrap();
    let (mut pos, mut vel) = (rounded.ticks[0], rounded.v0);
    let mut a = acc(&truth, pos).unwrap();
    let mut sim_vs_library_max = 0.0f64;
    for s in 1..rounded.ticks.len() {
        for _ in 0..N {
            step(&truth, &mut pos, &mut vel, &mut a).unwrap();
        }
        sim_vs_library_max = sim_vs_library_max
            .max((pos[0] - lib[s].x).abs())
            .max((pos[1] - lib[s].y).abs());
    }
    Check {
        kernel_truth_max,
        kernel_interp_max,
        sim_vs_library_max,
        sim_truth_max,
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick");
    args.retain(|a| a != "--quick");
    let threads = std::thread::available_parallelism().map_or(1, usize::from);
    match args.first().map(String::as_str) {
        Some("selfcheck") => {
            let c = selfcheck();
            println!(
                "W kernel on TRUE substep positions, whitened residual max over n=1,2,8,32: {:.3e}",
                c.kernel_truth_max
            );
            println!(
                "W kernel on Hermite-interpolated f64 observed ticks, same: {:.3e}",
                c.kernel_interp_max
            );
            println!("S on f64 truth start: {:.3e}", c.sim_truth_max);
            println!(
                "probe integrator vs simulation.rs, max abs tick position difference: {:.3e}",
                c.sim_vs_library_max
            );
        }
        Some("e1") => exp_e1(threads, quick),
        Some("isolation") => exp_isolation(threads, quick),
        Some("flat") => exp_flat(threads, quick),
        Some("paths") => exp_paths(threads, quick),
        Some("multi") => exp_multi(threads, quick),
        Some("captures") => exp_captures(threads),
        Some("timing") => exp_timing(),
        _ => {
            println!(
                "Usage: likelihood_value_probe <selfcheck|e1|isolation|flat|paths|multi|captures|timing> [--quick]\nOffline P3 experiments; see docs/likelihood-value-probe.md. Run from the repository root (captures read the local SQLite files)."
            );
        }
    }
    let _ = CELL_SIZE;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_kernel_reproduces_true_substep_positions_and_integrator_matches_library() {
        let c = selfcheck();
        assert!(
            c.kernel_truth_max < 1e-4,
            "kernel on true substeps: {}",
            c.kernel_truth_max
        );
        assert!(c.sim_truth_max < 1e-4, "S at truth: {}", c.sim_truth_max);
        assert!(
            c.sim_vs_library_max < 1e-9,
            "integrator: {}",
            c.sim_vs_library_max
        );
    }
}
