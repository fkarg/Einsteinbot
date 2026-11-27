//! The likelihood's noise model (P13): the exact measurement floor of each row (f32 rounding of
//! the ticks, whose rows share ticks, plus the white interpolation variance; `window.rs`) plus a
//! model-error process for the field of the planets not found yet.
//!
//! Model error: per path and axis a Matern-3/2 process in arc length along the row centres. Its
//! shape follows the field of unseen planets (Campbell's theorem: the variance of a Poisson field
//! of point masses grows as `1 / d^2`, `d` the distance to where they can be, and its correlation
//! length as `d`): at a row centre `c`, sd `scale * 100 / d_U(c)` and length
//! `length * d_U(c) / 100`, where `d_U` is the distance to the admissible region `U` of unseen
//! planet centres (`Admissible`), clamped below at `MIN_DISTANCE`. The two scalars are estimated
//! by type-II maximum a posteriori on prequential residuals (`Series`, `estimate`): each new
//! path's raw residuals against the maintained map before that path's refit, the last `WINDOW` of
//! them, with a weak log-normal hyperprior centred on `ModelError::PRIOR`. Validated by
//! `diagnose_matern_model_error` (`docs/estimator-experiments.md`, P13).
//!
//! The floor and the process are one linear-Gaussian state-space model per path and axis (state
//! `[f, f', n_r, n_r+1, n_r+2]`: the process, its slope and the rounding noise of the row's three
//! ticks), so the Kalman innovations whiten residuals and Jacobian columns exactly, with gains
//! that do not depend on the data (`Gains`). With the process off this is the exact Cholesky
//! whitening of the banded floor. A thinned path's floor is its rows' marginal (white) variance.
//!
//! The noise is frozen within an operation (`Noise`): a slice, a local refit, a polish or an audit
//! comparison computes it once from the maintained map and the paths, and every map it compares
//! uses the same noise, so evidence differences need no log-determinant.
use super::Shot;
use super::window::PathData;
use crate::protocol::Planet;
use crate::simulation::WorldParameters;

/// Prior (and starting) model-error scalars; `diagnose_matern_model_error` estimated
/// `scale` 0.056-0.10 and `length` 500-900, and the Campbell prior for 4 unseen planets gives
/// `scale` about 0.064.
const PRIOR_SCALE: f64 = 0.07;
const PRIOR_LENGTH: f64 = 600.0;
/// Standard deviation of the log-normal hyperprior of each scalar (weak: a factor e).
const HYPERPRIOR_LOG_SD: f64 = 1.0;
/// Prior of the white excess sd: log-normal around 1e-6 (log-sd 2), far below the f32 floor of a
/// row (about 3e-5 at coordinates near 1000), so exact captures are unaffected.
const PRIOR_WHITE: f64 = 1e-6;
const WHITE_LOG_SD: f64 = 2.0;
/// Prequential series the estimate uses (the most recent paths).
pub(super) const WINDOW: usize = 8;
/// `d_U` below this is clamped (planet radii are at least 20).
const MIN_DISTANCE: f64 = 20.0;
/// Raster of candidate centres: spacing, first node, and the clearance from the battlefield edge,
/// from the surface of a found planet and from an observed tick.
const SPACING: f64 = 10.0;
const ORIGIN: f64 = 25.0;
const CLEARANCE: f64 = 20.0;

/// The model-error scalars: row sd `scale * 100 / d_U`, length `length * d_U / 100`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ModelError {
    pub scale: f64,
    pub length: f64,
    /// Sd of a white excess noise added to every row on both axes (for position noise beyond the
    /// f32 rounding, which exact captures do not have).
    pub white: f64,
}

impl ModelError {
    pub const PRIOR: ModelError = ModelError {
        scale: PRIOR_SCALE,
        length: PRIOR_LENGTH,
        white: PRIOR_WHITE,
    };
    /// (sd, length) of the process at distance `d` from `U` (infinite without a `U`: no process).
    fn shape(&self, d: f64) -> (f64, f64) {
        let d = d.max(MIN_DISTANCE);
        (self.scale * 100.0 / d, self.length * d / 100.0)
    }
}

/// The admissible region `U` of unseen planet centres on a `SPACING` raster: inside the
/// battlefield by `CLEARANCE`, farther than `radius + CLEARANCE` from every found planet and
/// farther than `CLEARANCE` from every observed tick. Distances come from an exact Euclidean
/// distance transform with nearest-node tracking.
#[derive(Clone, Debug)]
pub(super) struct Admissible {
    nx: usize,
    ny: usize,
    /// Raster nodes before the first battlefield node, per axis.
    offset: [usize; 2],
    /// Per raster node, the nearest admissible node (`u32::MAX`: none at all).
    nearest: Vec<u32>,
    /// Area of the admissible region (admissible nodes times the node area).
    pub area: f64,
}

impl Admissible {
    pub fn new<'a>(
        planets: &[Planet],
        ticks: impl Iterator<Item = &'a [f64; 2]>,
        world: WorldParameters,
    ) -> Self {
        // Battlefield nodes `ORIGIN + SPACING * k` below `extent - CLEARANCE`, plus the margin.
        let count =
            |extent: f64| ((extent - CLEARANCE - ORIGIN) / SPACING).ceil().max(0.0) as usize;
        let inside = [
            count(world.battlefield_width),
            count(world.battlefield_height),
        ];
        let pad = ((world.margin + ORIGIN) / SPACING).ceil() as usize;
        let offset = [pad, pad];
        let (nx, ny) = (inside[0] + 2 * pad, inside[1] + 2 * pad);
        let mut free = vec![false; nx * ny];
        for j in pad..pad + inside[1] {
            free[j * nx + pad..j * nx + pad + inside[0]].fill(true);
        }
        let position = |k: usize, axis: usize| ORIGIN + SPACING * (k as f64 - offset[axis] as f64);
        let mut stamp = |x: f64, y: f64, radius: f64| {
            let range = |c: f64, axis: usize, n: usize| {
                let node = |v: f64| (v - ORIGIN) / SPACING + offset[axis] as f64;
                let lo = node(c - radius).ceil().clamp(0.0, n as f64) as usize;
                let hi = (node(c + radius).floor() + 1.0).clamp(0.0, n as f64) as usize;
                (lo, hi)
            };
            let ((i0, i1), (j0, j1)) = (range(x, 0, nx), range(y, 1, ny));
            for j in j0..j1 {
                for i in i0..i1 {
                    if (position(i, 0) - x).hypot(position(j, 1) - y) <= radius {
                        free[j * nx + i] = false;
                    }
                }
            }
        };
        for p in planets {
            stamp(p.x, p.y, p.radius + CLEARANCE);
        }
        for t in ticks {
            stamp(t[0], t[1], CLEARANCE);
        }
        Self {
            nx,
            ny,
            offset,
            nearest: nearest_free(&free, nx, ny),
            area: free.iter().filter(|f| **f).count() as f64 * SPACING * SPACING,
        }
    }

    fn node(&self, index: u32) -> [f64; 2] {
        let index = index as usize;
        [
            ORIGIN + SPACING * ((index % self.nx) as f64 - self.offset[0] as f64),
            ORIGIN + SPACING * ((index / self.nx) as f64 - self.offset[1] as f64),
        ]
    }

    /// Distance from `p` to the nearest admissible node (infinite when `U` is empty): the nearest
    /// of the nearest nodes of the raster nodes around `p`.
    pub fn distance(&self, p: [f64; 2]) -> f64 {
        let index = |c: f64, axis: usize, n: usize| {
            ((c - ORIGIN) / SPACING + self.offset[axis] as f64)
                .round()
                .clamp(0.0, (n - 1) as f64) as usize
        };
        let (i, j) = (index(p[0], 0, self.nx), index(p[1], 1, self.ny));
        let mut best = f64::INFINITY;
        for jj in j.saturating_sub(1)..(j + 2).min(self.ny) {
            for ii in i.saturating_sub(1)..(i + 2).min(self.nx) {
                let nearest = self.nearest[jj * self.nx + ii];
                if nearest != u32::MAX {
                    let q = self.node(nearest);
                    best = best.min((q[0] - p[0]).hypot(q[1] - p[1]));
                }
            }
        }
        best
    }
}

/// Exact Euclidean nearest-`true` transform of a raster (Felzenszwalb and Huttenlocher's two
/// passes, with the nearest node tracked): the index of the nearest free node per node.
fn nearest_free(free: &[bool], nx: usize, ny: usize) -> Vec<u32> {
    const NONE: usize = usize::MAX;
    // Pass 1, along x: the nearest free column in the same row.
    let mut column = vec![NONE; nx * ny];
    for j in 0..ny {
        let row = &mut column[j * nx..(j + 1) * nx];
        let mut last = NONE;
        for i in 0..nx {
            if free[j * nx + i] {
                last = i;
            }
            row[i] = last;
        }
        let mut next = NONE;
        for i in (0..nx).rev() {
            if free[j * nx + i] {
                next = i;
            }
            if next != NONE && (row[i] == NONE || next - i < i - row[i]) {
                row[i] = next;
            }
        }
    }
    // Pass 2, along y: lower envelope of the parabolas (j - q)^2 + g(q)^2.
    let mut nearest = vec![u32::MAX; nx * ny];
    let mut hull: Vec<usize> = Vec::with_capacity(ny);
    let mut start: Vec<f64> = Vec::with_capacity(ny + 1);
    for i in 0..nx {
        let g = |q: usize| -> Option<f64> {
            let c = column[q * nx + i];
            (c != NONE).then(|| (c as f64 - i as f64).powi(2))
        };
        hull.clear();
        start.clear();
        for q in 0..ny {
            let Some(fq) = g(q) else { continue };
            loop {
                let Some(&r) = hull.last() else {
                    hull.push(q);
                    start.push(f64::NEG_INFINITY);
                    break;
                };
                let fr = g(r).expect("only free columns enter the hull");
                let s = ((fq + (q * q) as f64) - (fr + (r * r) as f64)) / (2.0 * (q - r) as f64);
                if s <= *start.last().expect("one per hull entry") {
                    hull.pop();
                    start.pop();
                } else {
                    hull.push(q);
                    start.push(s);
                    break;
                }
            }
        }
        if hull.is_empty() {
            continue;
        }
        let mut k = 0;
        for j in 0..ny {
            while k + 1 < hull.len() && start[k + 1] < j as f64 {
                k += 1;
            }
            let q = hull[k];
            nearest[j * nx + i] = (q * nx + column[q * nx + i]) as u32;
        }
    }
    nearest
}

/// Expected number of planets (the local generator's default; the server does not send it).
const PLANETS_EXPECTED: f64 = 24.0;
/// The maintained map's unseen planets are `PLANETS_EXPECTED - K`, never fewer than this (a
/// complete map keeps a small allowance for planets beyond the default count).
const MIN_UNSEEN: f64 = 0.5;

/// The noise of one operation: the model-error scalars, the admissible region they are shaped by,
/// and the near cutoff `rho0`.
///
/// The process stands for the distant unseen planets only: with `N_u` unseen planets spread over
/// the admissible area `|U|`, the nearest one is typically `rho0 = sqrt(|U| / (pi N_u))` away, and
/// a single planet nearer than that is a localized signature for a birth to explain, not
/// background. So the distance in both the scale (`1 / d`) and the length (`d`) is
/// `max(d_U, rho0)`. Computed from the maintained map and frozen per operation.
#[derive(Clone, Debug)]
pub(super) struct Noise {
    pub model: ModelError,
    pub admissible: Admissible,
    pub rho0: f64,
}

impl Noise {
    /// The noise for the map `planets` (the maintained one) and the paths `shots`.
    pub fn new(model: ModelError, planets: &[Planet], shots: &[Shot]) -> Self {
        let ticks = shots
            .iter()
            .filter_map(|shot| shot.path.as_ref())
            .flat_map(|path| path.ticks.iter());
        let admissible = Admissible::new(planets, ticks, WorldParameters::default());
        let unseen = (PLANETS_EXPECTED - planets.len() as f64).max(MIN_UNSEEN);
        // An empty `U` (every candidate centre excluded) gives `rho0 = 0` and an infinite distance
        // everywhere: no process (sd 0, infinite length), so the gains carry the floor alone.
        let rho0 = (admissible.area / (std::f64::consts::PI * unseen)).sqrt();
        Self {
            model,
            admissible,
            rho0,
        }
    }

    /// Distance used for the process at `p`: to `U`, but at least `rho0`.
    pub fn distance(&self, p: [f64; 2]) -> f64 {
        self.admissible.distance(p).max(self.rho0)
    }

    /// (sd, length) of the process at `p`.
    pub fn shape(&self, p: [f64; 2]) -> (f64, f64) {
        self.model.shape(self.distance(p))
    }
}

/// `shots` with `noise` applied to every path.
pub(super) fn with_noise(shots: &[Shot], noise: &Noise) -> Vec<Shot> {
    let mut out = shots.to_vec();
    apply(&mut out, noise);
    out
}

pub(super) fn apply(shots: &mut [Shot], noise: &Noise) {
    for path in shots.iter_mut().filter_map(|shot| shot.path.as_mut()) {
        path.set_noise(noise);
    }
}

/// The measurement floor of a path's rows on both axes.
#[derive(Clone, Debug)]
pub(super) struct Floor {
    /// Rounding variance of every tick per axis; `None` for a thinned path, whose rows are
    /// treated as independent.
    pub ticks: Option<[Vec<f64>; 2]>,
    /// White variance per row and axis: the interpolation variance (full path) or the whole
    /// marginal floor variance (thinned path).
    pub white: [Vec<f64>; 2],
    /// Arc length along the path at each row centre.
    pub arc: Vec<f64>,
}

impl Floor {
    /// Marginal floor variance of row `r` on `axis`.
    pub fn marginal(&self, axis: usize, r: usize) -> f64 {
        self.white[axis][r]
            + self.ticks.as_ref().map_or(0.0, |v| {
                let v = &v[axis];
                v[r] + 4.0 * v[r + 1] + v[r + 2]
            })
    }
}

/// Kalman whitening of one axis of one path under a fixed noise: per row the transition of the
/// process state `(f, f')` from the previous row, the gain and the innovation's inverse sd.
#[derive(Clone, Debug, Default)]
pub(super) struct Gains {
    transition: Vec<[f64; 4]>,
    gain: Vec<[f64; 5]>,
    inverse_sd: Vec<f64>,
    /// `sum ln S_t` of the innovation variances.
    pub log_det: f64,
}

/// Observation of the state `[f, f', n_r, n_r+1, n_r+2]`: the row's second difference.
const H: [f64; 5] = [1.0, 0.0, 1.0, -2.0, 1.0];

impl Gains {
    /// Gains of `axis` of `floor` with the process `(sd, length)` per row.
    pub fn new(floor: &Floor, axis: usize, shape: &[(f64, f64)], excess: f64) -> Self {
        Self::with_missing(floor, axis, shape, excess, &[])
    }

    /// As `new`, where the rows with `missing[t]` true are not observed (predicted only: their
    /// whitened value is 0 and they add nothing to the log-determinant).
    /// `excess` is a white variance added to every row.
    fn with_missing(
        floor: &Floor,
        axis: usize,
        shape: &[(f64, f64)],
        excess: f64,
        missing: &[bool],
    ) -> Self {
        let n = floor.arc.len();
        let tick = |t: usize| floor.ticks.as_ref().map_or(0.0, |v| v[axis][t]);
        let mut out = Gains {
            transition: Vec::with_capacity(n),
            gain: Vec::with_capacity(n),
            inverse_sd: Vec::with_capacity(n),
            log_det: 0.0,
        };
        let mut p = [[0.0; 5]; 5];
        for t in 0..n {
            let (sd, length) = shape[t];
            let lambda = 3f64.sqrt() / length;
            let stationary = [sd * sd, lambda * lambda * sd * sd];
            let mut transition = [0.0; 4];
            if t == 0 {
                p[0][0] = stationary[0];
                p[1][1] = stationary[1];
                for k in 0..3 {
                    p[2 + k][2 + k] = tick(k);
                }
            } else {
                // The state is rescaled to this row's sd, then stepped by the stationary process.
                let previous = shape[t - 1].0;
                let ratio = if previous > 0.0 { sd / previous } else { 0.0 };
                let d = floor.arc[t] - floor.arc[t - 1];
                let e = (-lambda * d).exp();
                let f = [
                    e * (1.0 + lambda * d),
                    e * d,
                    -e * lambda * lambda * d,
                    e * (1.0 - lambda * d),
                ];
                transition = f.map(|v| v * ratio);
                // P <- F P F' for the sparse transition: (f, f') by `transition`, the tick noises
                // shift by one (n_r drops out) and a fresh one enters (its variance is added below).
                let tr = transition;
                let mut q = [[0.0; 5]; 5];
                for i in 0..2 {
                    let (a0, a1) = (tr[2 * i], tr[2 * i + 1]);
                    for k in 0..2 {
                        let (b0, b1) = (tr[2 * k], tr[2 * k + 1]);
                        q[i][k] =
                            a0 * (b0 * p[0][0] + b1 * p[0][1]) + a1 * (b0 * p[1][0] + b1 * p[1][1]);
                    }
                    // Cross terms with the shifted tick noises.
                    for j in 0..2 {
                        let v = a0 * p[0][3 + j] + a1 * p[1][3 + j];
                        q[i][2 + j] = v;
                        q[2 + j][i] = v;
                    }
                }
                for i in 0..2 {
                    for j in 0..2 {
                        q[2 + i][2 + j] = p[3 + i][3 + j];
                    }
                }
                p = q;
                // Process noise of the stationary step: P_inf - F P_inf F'.
                let q00 =
                    stationary[0] - (f[0] * f[0] * stationary[0] + f[1] * f[1] * stationary[1]);
                let q01 = -(f[0] * f[2] * stationary[0] + f[1] * f[3] * stationary[1]);
                let q11 =
                    stationary[1] - (f[2] * f[2] * stationary[0] + f[3] * f[3] * stationary[1]);
                p[0][0] += q00;
                p[0][1] += q01;
                p[1][0] += q01;
                p[1][1] += q11;
                p[4][4] += tick(t + 2);
            }
            if missing.get(t).copied().unwrap_or(false) {
                out.transition.push(transition);
                out.gain.push([0.0; 5]);
                out.inverse_sd.push(0.0);
                continue;
            }
            let ph: [f64; 5] = std::array::from_fn(|i| (0..5).map(|j| p[i][j] * H[j]).sum());
            let s = (0..5).map(|i| H[i] * ph[i]).sum::<f64>() + floor.white[axis][t] + excess;
            let gain = ph.map(|v| v / s);
            for i in 0..5 {
                for j in 0..5 {
                    p[i][j] -= gain[i] * ph[j];
                }
            }
            out.transition.push(transition);
            out.gain.push(gain);
            out.inverse_sd.push(s.sqrt().recip());
            out.log_det += s.ln();
        }
        out
    }

    /// Replace `v` (one value per row) by its normalized innovations.
    pub fn whiten(&self, v: &mut [f64]) {
        let mut m = [0.0; 5];
        for (t, value) in v.iter_mut().enumerate() {
            if t > 0 {
                let a = &self.transition[t];
                m = [
                    a[0] * m[0] + a[1] * m[1],
                    a[2] * m[0] + a[3] * m[1],
                    m[3],
                    m[4],
                    0.0,
                ];
            }
            let innovation = *value - (m[0] + m[2] - 2.0 * m[3] + m[4]);
            *value = innovation * self.inverse_sd[t];
            let k = &self.gain[t];
            for i in 0..5 {
                m[i] += k[i] * innovation;
            }
        }
    }
}

/// One path's prequential residuals: its raw residuals against the maintained map before the
/// path was fitted, with its floor and the distance of each row centre to `U` at that moment.
#[derive(Clone, Debug)]
pub(super) struct Series {
    floor: Floor,
    distance: Vec<f64>,
    raw: [Vec<f64>; 2],
    /// Rows within `CLEARANCE` of a map planet's surface: their residual is dominated by
    /// that planet's parameter error (or by a wrong planet), not by the field of unseen planets,
    /// so the estimate treats them as missing.
    missing: Vec<bool>,
}

impl Series {
    pub fn new(path: &PathData, planets: &[Planet], noise: &Noise) -> Self {
        let rows = path.row_count();
        let mut prediction = [vec![0.0; rows], vec![0.0; rows]];
        for p in planets {
            path.add_planet(p.x, p.y, p.mass, &mut prediction);
        }
        let raw = [0, 1].map(|axis| {
            let mut out = vec![0.0; rows];
            path.raw_residual(axis, &prediction[axis], &mut out);
            out
        });
        let missing = (0..rows)
            .map(|r| {
                let c = path.center(r);
                planets
                    .iter()
                    .any(|p| (p.x - c[0]).hypot(p.y - c[1]) <= p.radius + CLEARANCE)
            })
            .collect();
        Self {
            floor: path.floor().clone(),
            distance: (0..rows).map(|r| noise.distance(path.center(r))).collect(),
            raw,
            missing,
        }
    }

    /// Gaussian log-likelihood of the residuals under `model` (without the `2 pi` constant).
    pub fn log_likelihood(&self, model: ModelError) -> f64 {
        self.innovations(model)
            .into_iter()
            .map(|(v, log_det)| -0.5 * (v.iter().map(|x| x * x).sum::<f64>() + log_det))
            .sum()
    }

    /// Normalized innovations of each axis under `model` (0 at missing rows) and their
    /// log-determinant.
    pub fn innovations(&self, model: ModelError) -> [(Vec<f64>, f64); 2] {
        let shape: Vec<(f64, f64)> = self.distance.iter().map(|d| model.shape(*d)).collect();
        [0, 1].map(|axis| {
            let gains = Gains::with_missing(
                &self.floor,
                axis,
                &shape,
                model.white * model.white,
                &self.missing,
            );
            let mut v = self.raw[axis].clone();
            gains.whiten(&mut v);
            (v, gains.log_det)
        })
    }

    /// Also treat the rows with `missing[r]` true as missing.
    #[cfg(test)]
    pub fn drop_rows(&mut self, missing: &[bool]) {
        for (m, extra) in self.missing.iter_mut().zip(missing) {
            *m |= *extra;
        }
    }

    #[cfg(test)]
    pub fn missing(&self) -> &[bool] {
        &self.missing
    }
}

/// The type-II MAP scalars (the prior without any series). The background's `scale` and
/// `length` come from the `earlier` paths' series only, so a path is never fitted under a
/// background widened by its own signature; the white excess comes from a joint estimate on those
/// and the `current` path's series (the background free there too, so a smooth planet signature
/// is taken by it, not by the white term): white position noise is a property of the
/// observations, not of the map. Pattern searches
/// in log space from the prior, so the estimate is a function of the series alone.
pub(super) fn estimate(earlier: &[Series], current: Option<&Series>) -> ModelError {
    let centre = [PRIOR_SCALE.ln(), PRIOR_LENGTH.ln(), PRIOR_WHITE.ln()];
    let sd = [HYPERPRIOR_LOG_SD, HYPERPRIOR_LOG_SD, WHITE_LOG_SD];
    // The scale stays below about six times the Campbell value for all 24 planets unseen.
    let bounds = [
        (1e-6f64.ln(), 1f64.ln()),
        (20f64.ln(), 1e5f64.ln()),
        (1e-9f64.ln(), 1f64.ln()),
    ];
    let model_of = |u: [f64; 3]| ModelError {
        scale: u[0].exp(),
        length: u[1].exp(),
        white: u[2].exp(),
    };
    let search = |series: &[&Series], free: &[usize], start: [f64; 3]| -> [f64; 3] {
        let objective = |u: [f64; 3]| -> f64 {
            let prior: f64 = (0..3)
                .map(|i| -0.5 * ((u[i] - centre[i]) / sd[i]).powi(2))
                .sum();
            series
                .iter()
                .map(|s| s.log_likelihood(model_of(u)))
                .sum::<f64>()
                + prior
        };
        let mut u = start;
        let mut best = objective(u);
        // The likelihood is flat in the white sd far below the floor, so a local search from
        // the prior would never leave it: scan it by decades first.
        if free.contains(&2) {
            for k in 0..9 {
                let mut trial = u;
                trial[2] = (1e-9f64 * 10f64.powi(k)).ln();
                let value = objective(trial);
                if value > best {
                    (u, best) = (trial, value);
                }
            }
        }
        let mut step = 1.0;
        while step > 0.02 {
            let mut moved = false;
            for &axis in free {
                for sign in [1.0, -1.0] {
                    let mut trial = u;
                    trial[axis] = (trial[axis] + sign * step).clamp(bounds[axis].0, bounds[axis].1);
                    let value = objective(trial);
                    if value > best {
                        (u, best, moved) = (trial, value, true);
                    }
                }
            }
            if !moved {
                step /= 2.0;
            }
        }
        u
    };
    let earlier: Vec<&Series> = earlier.iter().collect();
    let mut u = if earlier.is_empty() {
        centre
    } else {
        search(&earlier, &[0, 1, 2], centre)
    };
    if let Some(current) = current {
        // All three free here, so a smooth signature on the current path goes to the background
        // in this estimate and only what is not smooth to the white term; only the latter is kept.
        let all: Vec<&Series> = earlier.iter().copied().chain([current]).collect();
        u[2] = search(&all, &[0, 1, 2], u)[2];
    }
    model_of(u)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::fit::planet_at;
    use crate::protocol::Point;
    use nalgebra::DMatrix;

    fn path() -> PathData {
        let points: Vec<Point> = (0..40)
            .map(|i| Point {
                x: 1000.0 + 3.0 * i as f32 + 0.01 * (i * i) as f32,
                y: 20.0 + 5.0 * i as f32 + 0.002 * (i * i * i) as f32,
            })
            .collect();
        PathData::new(&points, 0.0, 3.0, WorldParameters::default()).unwrap()
    }

    /// The banded floor covariance of one axis plus `diagonal` on the diagonal.
    fn floor_covariance(floor: &Floor, axis: usize, diagonal: f64) -> DMatrix<f64> {
        let n = floor.arc.len();
        let ticks = &floor.ticks.as_ref().unwrap()[axis];
        DMatrix::from_fn(n, n, |i, j| {
            let coefs = |r: usize| [(r, 1.0), (r + 1, -2.0), (r + 2, 1.0)];
            let mut c = if i == j {
                floor.white[axis][i] + diagonal
            } else {
                0.0
            };
            for (ti, ci) in coefs(i) {
                for (tj, cj) in coefs(j) {
                    if ti == tj {
                        c += ci * cj * ticks[ti];
                    }
                }
            }
            c
        })
    }

    /// With a varying process shape, missing rows and a white excess, the innovations at the
    /// observed rows are the dense Cholesky whitening of the observed rows' joint covariance (the
    /// process covariance built here by propagating the state covariance and transitions, not by
    /// filtering), missing rows give 0, and the log-determinant is that covariance's.
    #[test]
    fn whitening_matches_the_dense_covariance_with_varying_shape_missing_rows_and_excess() {
        let path = path();
        let floor = path.floor();
        let n = floor.arc.len();
        let axis = 0;
        let shape: Vec<(f64, f64)> = (0..n)
            .map(|t| {
                let t = t as f64;
                (1e-3 * (1.0 + 0.5 * (0.3 * t).sin()), 30.0 + 2.0 * t)
            })
            .collect();
        let excess = 4e-8;
        let missing: Vec<bool> = (0..n).map(|t| t % 7 == 3).collect();
        // Process: x_t = A_t x_(t-1) + w_t, the transitions and stationary variances of `Gains`.
        type M2 = [[f64; 2]; 2];
        let mul = |a: M2, b: M2| -> M2 {
            let mut c = [[0.0; 2]; 2];
            for i in 0..2 {
                for j in 0..2 {
                    c[i][j] = a[i][0] * b[0][j] + a[i][1] * b[1][j];
                }
            }
            c
        };
        let transpose = |a: M2| -> M2 { [[a[0][0], a[1][0]], [a[0][1], a[1][1]]] };
        let mut step: Vec<M2> = vec![[[0.0; 2]; 2]; n];
        let mut var: Vec<M2> = vec![[[0.0; 2]; 2]; n];
        for t in 0..n {
            let (sd, length) = shape[t];
            let lambda = 3f64.sqrt() / length;
            let stationary = [[sd * sd, 0.0], [0.0, lambda * lambda * sd * sd]];
            if t == 0 {
                var[0] = stationary;
                continue;
            }
            let d = floor.arc[t] - floor.arc[t - 1];
            let e = (-lambda * d).exp();
            let f = [
                [e * (1.0 + lambda * d), e * d],
                [-e * lambda * lambda * d, e * (1.0 - lambda * d)],
            ];
            let ratio = sd / shape[t - 1].0;
            step[t] = [
                [f[0][0] * ratio, f[0][1] * ratio],
                [f[1][0] * ratio, f[1][1] * ratio],
            ];
            let carried = mul(mul(step[t], var[t - 1]), transpose(step[t]));
            let fsf = mul(mul(f, stationary), transpose(f));
            for i in 0..2 {
                for j in 0..2 {
                    var[t][i][j] = carried[i][j] + stationary[i][j] - fsf[i][j];
                }
            }
        }
        let mut covariance = floor_covariance(floor, axis, excess);
        for j in 0..n {
            let mut propagated = var[j];
            for i in j..n {
                if i > j {
                    propagated = mul(step[i], propagated);
                }
                covariance[(i, j)] += propagated[0][0];
                if i != j {
                    covariance[(j, i)] += propagated[0][0];
                }
            }
        }
        let observed: Vec<usize> = (0..n).filter(|t| !missing[*t]).collect();
        let sub = DMatrix::from_fn(observed.len(), observed.len(), |i, j| {
            covariance[(observed[i], observed[j])]
        });
        let cholesky = sub.cholesky().unwrap();
        let v: Vec<f64> = (0..n)
            .map(|i| ((i * 37) % 11) as f64 * 1e-4 - 5e-4)
            .collect();
        let v_observed: Vec<f64> = observed.iter().map(|&t| v[t]).collect();
        let expected = cholesky
            .l()
            .solve_lower_triangular(&DMatrix::from_column_slice(observed.len(), 1, &v_observed))
            .unwrap();
        let gains = Gains::with_missing(floor, axis, &shape, excess, &missing);
        let mut got = v.clone();
        gains.whiten(&mut got);
        let mut k = 0;
        for t in 0..n {
            if missing[t] {
                assert_eq!(got[t], 0.0, "row {t}");
                continue;
            }
            assert!(
                (got[t] - expected[k]).abs() <= 1e-6 * expected[k].abs().max(1.0),
                "row {t}: {} vs {}",
                got[t],
                expected[k]
            );
            k += 1;
        }
        let log_det: f64 = 2.0
            * (0..observed.len())
                .map(|i| cholesky.l()[(i, i)].ln())
                .sum::<f64>();
        assert!(
            (gains.log_det - log_det).abs() < 1e-6 * log_det.abs(),
            "{} vs {log_det}",
            gains.log_det
        );
    }

    /// The estimate recovers an injected white position noise and keeps the white sd far below
    /// the floor on exact residuals.
    #[test]
    fn estimate_recovers_white_noise_and_ignores_it_on_exact_rows() {
        let path = path();
        let floor = path.floor().clone();
        let n = floor.arc.len();
        let mut state = 11u64;
        let mut gaussian = || {
            (0..12)
                .map(|_| {
                    state = state
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    (state >> 11) as f64 / (1u64 << 53) as f64
                })
                .sum::<f64>()
                - 6.0
        };
        let series = |raw: [Vec<f64>; 2]| Series {
            floor: floor.clone(),
            // No admissible region: no background process.
            distance: vec![f64::INFINITY; n],
            raw,
            missing: vec![false; n],
        };
        let injected = 2e-3;
        let noisy = series([0, 1].map(|_| (0..n).map(|_| injected * gaussian()).collect()));
        let white = estimate(&[], Some(&noisy)).white;
        assert!(
            (white / injected - 1.0).abs() < 0.3,
            "injected {injected}, estimated {white}"
        );
        // Exact rows: the floor's own noise (rounding of every tick, second-differenced).
        let ticks = floor.ticks.clone().unwrap();
        let exact = series([0, 1].map(|axis| {
            let rounding: Vec<f64> = ticks[axis].iter().map(|v| v.sqrt() * gaussian()).collect();
            (0..n)
                .map(|r| rounding[r] - 2.0 * rounding[r + 1] + rounding[r + 2])
                .collect()
        }));
        let white = estimate(&[], Some(&exact)).white;
        let floor_sd = floor.marginal(0, 0).sqrt();
        assert!(white < 0.3 * floor_sd, "white {white} floor {floor_sd}");
    }

    /// Without the process the innovations are the exact Cholesky whitening of the banded floor
    /// covariance (rounding noise shared by neighbouring rows plus white interpolation variance),
    /// and the log-determinant is that of the covariance.
    #[test]
    fn floor_alone_is_the_exact_banded_whitening() {
        let path = path();
        let floor = path.floor();
        let n = floor.arc.len();
        let shape = vec![(0.0, f64::INFINITY); n];
        for axis in 0..2 {
            let ticks = &floor.ticks.as_ref().unwrap()[axis];
            let covariance = DMatrix::from_fn(n, n, |i, j| {
                let coefs = |r: usize| [(r, 1.0), (r + 1, -2.0), (r + 2, 1.0)];
                let mut c = if i == j { floor.white[axis][i] } else { 0.0 };
                for (ti, ci) in coefs(i) {
                    for (tj, cj) in coefs(j) {
                        if ti == tj {
                            c += ci * cj * ticks[ti];
                        }
                    }
                }
                c
            });
            let cholesky = covariance.clone().cholesky().unwrap();
            let v: Vec<f64> = (0..n).map(|i| ((i * 7919) % 13) as f64 * 1e-5).collect();
            let expected = cholesky
                .l()
                .solve_lower_triangular(&DMatrix::from_column_slice(n, 1, &v))
                .unwrap();
            let gains = Gains::new(floor, axis, &shape, 0.0);
            let mut got = v.clone();
            gains.whiten(&mut got);
            for i in 0..n {
                assert!(
                    (got[i] - expected[i]).abs() <= 1e-9 * expected[i].abs().max(1.0),
                    "axis {axis} row {i}: {} vs {}",
                    got[i],
                    expected[i]
                );
            }
            let log_det = 2.0 * (0..n).map(|i| cholesky.l()[(i, i)].ln()).sum::<f64>();
            assert!((gains.log_det - log_det).abs() < 1e-8 * log_det.abs());
        }
    }

    /// With the process on, the innovations whiten the sum of the floor and the Matern-3/2
    /// covariance (`s^2 (1 + sqrt3 d / l) exp(-sqrt3 d / l)` for a stationary shape).
    #[test]
    fn process_and_floor_whiten_their_joint_covariance() {
        let path = path();
        let floor = path.floor();
        let n = floor.arc.len();
        let (sd, length) = (1e-3, 40.0);
        let shape = vec![(sd, length); n];
        let axis = 1;
        let ticks = &floor.ticks.as_ref().unwrap()[axis];
        let covariance = DMatrix::from_fn(n, n, |i, j| {
            let coefs = |r: usize| [(r, 1.0), (r + 1, -2.0), (r + 2, 1.0)];
            let mut c = if i == j { floor.white[axis][i] } else { 0.0 };
            for (ti, ci) in coefs(i) {
                for (tj, cj) in coefs(j) {
                    if ti == tj {
                        c += ci * cj * ticks[ti];
                    }
                }
            }
            let x = 3f64.sqrt() * (floor.arc[i] - floor.arc[j]).abs() / length;
            c + sd * sd * (1.0 + x) * (-x).exp()
        });
        let cholesky = covariance.cholesky().unwrap();
        let v: Vec<f64> = (0..n).map(|i| ((i * 31) % 7) as f64 * 1e-4).collect();
        let expected = cholesky
            .l()
            .solve_lower_triangular(&DMatrix::from_column_slice(n, 1, &v))
            .unwrap();
        let gains = Gains::new(floor, axis, &shape, 0.0);
        let mut got = v.clone();
        gains.whiten(&mut got);
        for i in 0..n {
            assert!(
                (got[i] - expected[i]).abs() <= 1e-6 * expected[i].abs().max(1.0),
                "row {i}: {} vs {}",
                got[i],
                expected[i]
            );
        }
    }

    /// The distance transform gives the brute-force distance to the nearest admissible node.
    #[test]
    fn admissible_distance_matches_brute_force() {
        let world = WorldParameters::default();
        let planets = vec![
            planet_at([400.0, 300.0, 35.0]),
            planet_at([1200.0, 700.0, 25.0]),
        ];
        let ticks: Vec<[f64; 2]> = (0..600)
            .map(|i| {
                [
                    100.0 + 4.0 * i as f64,
                    500.0 + 120.0 * (i as f64 / 40.0).sin(),
                ]
            })
            .chain((0..300).map(|i| [-200.0 + 3.0 * i as f64, 1300.0 - 2.5 * i as f64]))
            .collect();
        let admissible = Admissible::new(&planets, ticks.iter(), world);
        let mut nodes = Vec::new();
        let mut y = ORIGIN;
        while y < world.battlefield_height - CLEARANCE {
            let mut x = ORIGIN;
            while x < world.battlefield_width - CLEARANCE {
                if planets
                    .iter()
                    .all(|p| (p.x - x).hypot(p.y - y) > p.radius + CLEARANCE)
                    && ticks.iter().all(|t| (t[0] - x).hypot(t[1] - y) > CLEARANCE)
                {
                    nodes.push([x, y]);
                }
                x += SPACING;
            }
            y += SPACING;
        }
        let mut state = 5u64;
        let mut worst: f64 = 0.0;
        for _ in 0..2000 {
            let mut uniform = || {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 11) as f64 / (1u64 << 53) as f64
            };
            // The battlefield and the margin that paths cross.
            let p = [
                (uniform() * 1.4 - 0.2) * world.battlefield_width,
                (uniform() * 1.6 - 0.3) * world.battlefield_height,
            ];
            let brute = nodes
                .iter()
                .map(|n| (n[0] - p[0]).hypot(n[1] - p[1]))
                .fold(f64::INFINITY, f64::min);
            worst = worst.max((admissible.distance(p) - brute).abs());
        }
        // Ticks themselves (row centres lie on the path).
        for t in &ticks {
            let brute = nodes
                .iter()
                .map(|n| (n[0] - t[0]).hypot(n[1] - t[1]))
                .fold(f64::INFINITY, f64::min);
            worst = worst.max((admissible.distance(*t) - brute).abs());
        }
        assert!(worst < 1e-9, "worst difference {worst}");
        let empty = Admissible::new(
            &[],
            std::iter::empty(),
            WorldParameters {
                battlefield_width: 30.0,
                battlefield_height: 30.0,
                ..world
            },
        );
        assert_eq!(empty.distance([10.0, 10.0]), f64::INFINITY);
    }
}
