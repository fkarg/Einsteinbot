//! Residual correction for aiming (P17): the conditional mean of the model-error process
//! (`noise.rs`) along each observed path, spread to nearby space for the targeting simulation.
//!
//! Along a path, the residual of every row against the published map is the model-error process
//! plus the measurement floor; both are the linear-Gaussian state-space model of `noise.rs`
//! (state `[f, f', n_r, n_r+1, n_r+2]`), so a Kalman filter and Rauch-Tung-Striebel smoother give
//! the exact posterior mean of the process `f` at each row centre (`smooth`). Rows within
//! `CLEARANCE` of a map planet's surface are not observed, as in the model-error estimate.
//!
//! Off the paths there is no exact model (the process is one-dimensional per path), so the
//! smoothed values are spread by a heuristic on a 15-unit raster (`spread`): the average of the
//! rows in and next to the nearest cell that holds rows, times the Matern-3/2 correlation of the
//! distance to that cell at the process length. It is strongest on observed rows, decays to zero
//! with distance, and is zero inside (and one raster cell around) every map planet. The process
//! length is the median over the rows of `length * max(d_U, rho0) / 100`.
//!
//! Gate: when the largest process sd anywhere (`scale * 100 / max(rho0, 20)`) or the largest
//! correction is below `GATE_ACCELERATION`, nothing is built or applied (`ResidualGate`).
use super::Shot;
use super::noise::{ModelError, Noise};
use super::window::PathData;
use super::work::par_map;
use crate::map::Vec2;
use crate::protocol::Planet;
use crate::simulation::WorldParameters;
use nalgebra::{Matrix5, Vector5};

/// An acceleration that moves a shot by about one unit over 100 ticks (`a t^2 / 2`), about five
/// times the f32 rounding floor of a row near coordinate 1000. Below it a correction cannot move
/// an aimed shot enough to matter.
pub const GATE_ACCELERATION: f64 = 2e-4;
/// Rows this close to a map planet's surface are not observed (`noise::Series`).
const CLEARANCE: f64 = 20.0;
/// `d_U` clamp of the process shape (`noise.rs`).
const MIN_DISTANCE: f64 = 20.0;
/// Raster cell size (world units).
const CELL: f64 = 15.0;

/// The model-error scalars the correction was built with (serialized with the belief, so a
/// recorded decision can rebuild the same correction from its belief and the capture's paths).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResidualModel {
    pub scale: f64,
    pub length: f64,
    pub white: f64,
}

impl From<ModelError> for ResidualModel {
    fn from(m: ModelError) -> Self {
        Self {
            scale: m.scale,
            length: m.length,
            white: m.white,
        }
    }
}

impl From<ResidualModel> for ModelError {
    fn from(m: ResidualModel) -> Self {
        Self {
            scale: m.scale,
            length: m.length,
            white: m.white,
        }
    }
}

/// Why no correction is applied. Not applied is not zero model error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResidualGate {
    /// The belief did not come from the estimator (oracle map, replay without the paths).
    NotComputed,
    /// The largest process sd is below `GATE_ACCELERATION` (complete or nearly complete map):
    /// nothing was computed.
    ScaleBelowThreshold,
    /// Built, but the largest correction is below `GATE_ACCELERATION`: not applied.
    MagnitudeBelowThreshold,
    /// No path has an observed row, or the process is off (no admissible region).
    NoEvidence,
}

/// The correction field on a raster over the battlefield and its margin (bilinear in between,
/// zero outside). Acceleration units (position per tick squared), like `PlanetMap::acceleration`.
#[derive(Clone, Debug)]
pub struct ResidualCorrection {
    origin: [f64; 2],
    cell: f64,
    nx: usize,
    ny: usize,
    values: Vec<[f64; 2]>,
    /// The process length the field decays at (world units).
    pub length: f64,
    /// Largest correction magnitude on the raster.
    pub max_magnitude: f64,
    /// Observed rows that contributed.
    pub rows: usize,
    pub model: ResidualModel,
}

impl ResidualCorrection {
    /// The correction at `p` (zero outside the raster).
    pub fn at(&self, p: Vec2) -> Vec2 {
        let u = (p.x - self.origin[0]) / self.cell;
        let v = (p.y - self.origin[1]) / self.cell;
        if !(u >= 0.0 && v >= 0.0) {
            return Vec2::default();
        }
        let (i, j) = (u.floor() as usize, v.floor() as usize);
        if i + 1 >= self.nx || j + 1 >= self.ny {
            return Vec2::default();
        }
        let (fu, fv) = (u - i as f64, v - j as f64);
        let at = |i: usize, j: usize| self.values[j * self.nx + i];
        let (a, b, c, d) = (at(i, j), at(i + 1, j), at(i, j + 1), at(i + 1, j + 1));
        let mix = |k: usize| {
            (a[k] * (1.0 - fu) + b[k] * fu) * (1.0 - fv) + (c[k] * (1.0 - fu) + d[k] * fu) * fv
        };
        Vec2::new(mix(0), mix(1))
    }
}

/// Largest process sd anywhere under `noise` (0 without an admissible region).
fn peak_sd(noise: &Noise) -> f64 {
    if noise.rho0 <= 0.0 || !noise.rho0.is_finite() {
        return 0.0;
    }
    noise.model.scale * 100.0 / noise.rho0.max(MIN_DISTANCE)
}

/// One observed row: its centre, the smoothed process (both axes) and the process length there.
struct Row {
    center: [f64; 2],
    mean: [f64; 2],
    length: f64,
}

/// The correction for the map `planets` from the paths of `shots` under `noise` (built from the
/// same map and paths), on up to `threads` threads (the result does not depend on them).
pub(super) fn build(
    planets: &[Planet],
    shots: &[Shot],
    noise: &Noise,
    threads: usize,
) -> Result<ResidualCorrection, ResidualGate> {
    if peak_sd(noise) < GATE_ACCELERATION {
        return Err(if noise.rho0 > 0.0 {
            ResidualGate::ScaleBelowThreshold
        } else {
            ResidualGate::NoEvidence
        });
    }
    let excess = noise.model.white * noise.model.white;
    let paths: Vec<&PathData> = shots.iter().filter_map(|shot| shot.path.as_ref()).collect();
    // Paths are smoothed independently, so in parallel; the rows keep the path order.
    let rows: Vec<Row> = par_map(&paths, threads, |path| {
        let n = path.row_count();
        let mut prediction = [vec![0.0; n], vec![0.0; n]];
        for p in planets {
            path.add_planet(p.x, p.y, p.mass, &mut prediction);
        }
        let centers: Vec<[f64; 2]> = (0..n).map(|r| path.center(r)).collect();
        let missing: Vec<bool> = centers
            .iter()
            .map(|c| {
                planets
                    .iter()
                    .any(|p| (p.x - c[0]).hypot(p.y - c[1]) <= p.radius + CLEARANCE)
            })
            .collect();
        if missing.iter().all(|m| *m) {
            return Vec::new();
        }
        let shape: Vec<(f64, f64)> = centers.iter().map(|c| noise.shape(*c)).collect();
        let means = [0, 1].map(|axis| {
            let mut raw = vec![0.0; n];
            path.raw_residual(axis, &prediction[axis], &mut raw);
            let ticks = path
                .floor()
                .ticks
                .as_ref()
                .map(|v| v[axis].clone())
                .unwrap_or_else(|| vec![0.0; n + 2]);
            let white: Vec<f64> = path.floor().white[axis]
                .iter()
                .map(|w| w + excess)
                .collect();
            smooth(&path.floor().arc, &shape, &ticks, &white, &missing, &raw)
        });
        (0..n)
            .filter(|r| !missing[*r])
            .map(|r| Row {
                center: centers[r],
                mean: [means[0][r], means[1][r]],
                length: shape[r].1,
            })
            .collect()
    })
    .into_iter()
    .flatten()
    .collect();
    if rows.is_empty() {
        return Err(ResidualGate::NoEvidence);
    }
    let mut lengths: Vec<f64> = rows.iter().map(|r| r.length).collect();
    lengths.sort_by(f64::total_cmp);
    let length = lengths[lengths.len() / 2];
    let correction = spread(&rows, length, planets, noise.model.into());
    if correction.max_magnitude < GATE_ACCELERATION {
        return Err(ResidualGate::MagnitudeBelowThreshold);
    }
    Ok(correction)
}

/// Posterior mean of the process `f` at each row of one path axis (Kalman filter and RTS
/// smoother of the `noise.rs` state-space model). `shape` is the process (sd, length) per row,
/// `ticks` the rounding variance per tick (rows + 2), `white` the white variance per row; rows
/// with `missing` are not observed.
fn smooth(
    arc: &[f64],
    shape: &[(f64, f64)],
    ticks: &[f64],
    white: &[f64],
    missing: &[bool],
    y: &[f64],
) -> Vec<f64> {
    let n = arc.len();
    let h = Vector5::new(1.0, 0.0, 1.0, -2.0, 1.0);
    let mut transitions = Vec::with_capacity(n);
    let mut predicted: Vec<(Vector5<f64>, Matrix5<f64>)> = Vec::with_capacity(n);
    let mut filtered: Vec<(Vector5<f64>, Matrix5<f64>)> = Vec::with_capacity(n);
    for t in 0..n {
        let (sd, length) = shape[t];
        let lambda = 3f64.sqrt() / length;
        let stationary = [sd * sd, lambda * lambda * sd * sd];
        let (mut m, mut p) = if t == 0 {
            transitions.push(Matrix5::zeros());
            let p = Matrix5::from_diagonal(&Vector5::new(
                stationary[0],
                stationary[1],
                ticks[0],
                ticks[1],
                ticks[2],
            ));
            (Vector5::zeros(), p)
        } else {
            let previous = shape[t - 1].0;
            let ratio = if previous > 0.0 { sd / previous } else { 0.0 };
            let d = arc[t] - arc[t - 1];
            let e = (-lambda * d).exp();
            let f = [
                e * (1.0 + lambda * d),
                e * d,
                -e * lambda * lambda * d,
                e * (1.0 - lambda * d),
            ];
            let mut a = Matrix5::zeros();
            a[(0, 0)] = f[0] * ratio;
            a[(0, 1)] = f[1] * ratio;
            a[(1, 0)] = f[2] * ratio;
            a[(1, 1)] = f[3] * ratio;
            a[(2, 3)] = 1.0;
            a[(3, 4)] = 1.0;
            let mut q = Matrix5::zeros();
            q[(0, 0)] = stationary[0] - (f[0] * f[0] * stationary[0] + f[1] * f[1] * stationary[1]);
            q[(0, 1)] = -(f[0] * f[2] * stationary[0] + f[1] * f[3] * stationary[1]);
            q[(1, 0)] = q[(0, 1)];
            q[(1, 1)] = stationary[1] - (f[2] * f[2] * stationary[0] + f[3] * f[3] * stationary[1]);
            q[(4, 4)] = ticks[t + 2];
            let (mf, pf) = &filtered[t - 1];
            let m = a * mf;
            let p = a * pf * a.transpose() + q;
            transitions.push(a);
            (m, p)
        };
        predicted.push((m, p));
        if !missing[t] {
            let ph = p * h;
            let s = h.dot(&ph) + white[t];
            let gain = ph / s;
            m += gain * (y[t] - h.dot(&m));
            p -= gain * ph.transpose();
            p = (p + p.transpose()) * 0.5;
        }
        filtered.push((m, p));
    }
    let mut smoothed = vec![Vector5::zeros(); n];
    smoothed[n - 1] = filtered[n - 1].0;
    for t in (0..n - 1).rev() {
        let (mf, pf) = &filtered[t];
        let (mp, pp) = &predicted[t + 1];
        let a = &transitions[t + 1];
        // C = P_f A' P_p^-1, by solving P_p C' = A P_f (P_p symmetric).
        let rhs = a * pf;
        let gain_t = pp
            .clone()
            .cholesky()
            .map(|c| c.solve(&rhs))
            .or_else(|| pp.clone().pseudo_inverse(1e-300).ok().map(|inv| inv * rhs));
        smoothed[t] = match gain_t {
            Some(ct) => mf + ct.transpose() * (smoothed[t + 1] - mp),
            None => *mf,
        };
    }
    smoothed.iter().map(|m| m[0]).collect()
}

/// Spread the row values over a raster of the battlefield and its margin: each cell holding rows
/// gets the Gaussian-weighted average (sd one cell) of the rows around it; every other cell the
/// value of the nearest such cell times the Matern-3/2 correlation of the distance to it at
/// `length`. Zero within one cell of a planet disk.
fn spread(
    rows: &[Row],
    length: f64,
    planets: &[Planet],
    model: ResidualModel,
) -> ResidualCorrection {
    let world = WorldParameters::default();
    let cell = CELL;
    let origin = [-world.margin, -world.margin];
    let nx = ((world.battlefield_width + 2.0 * world.margin) / cell).ceil() as usize + 1;
    let ny = ((world.battlefield_height + 2.0 * world.margin) / cell).ceil() as usize + 1;
    let index = |c: [f64; 2]| -> Option<usize> {
        let i = ((c[0] - origin[0]) / cell).round();
        let j = ((c[1] - origin[1]) / cell).round();
        (i >= 0.0 && j >= 0.0 && (i as usize) < nx && (j as usize) < ny)
            .then(|| j as usize * nx + i as usize)
    };
    // Channels: weight, weighted x, weighted y.
    let mut grid = vec![[0.0f64; 3]; nx * ny];
    let mut observed = vec![false; nx * ny];
    for row in rows {
        if let Some(k) = index(row.center) {
            grid[k][0] += 1.0;
            grid[k][1] += row.mean[0];
            grid[k][2] += row.mean[1];
            observed[k] = true;
        }
    }
    blur(&mut grid, nx, ny, 1.0);
    let nearest = nearest_seed(&observed, nx, ny);
    let mut values = vec![[0.0; 2]; nx * ny];
    let mut max_magnitude: f64 = 0.0;
    let root3 = 3f64.sqrt();
    for j in 0..ny {
        for i in 0..nx {
            let k = j * nx + i;
            let Some(seed) = nearest[k] else { continue };
            let x = [origin[0] + i as f64 * cell, origin[1] + j as f64 * cell];
            if planets
                .iter()
                .any(|p| (p.x - x[0]).hypot(p.y - x[1]) <= p.radius + cell)
            {
                continue;
            }
            let (si, sj) = (seed % nx, seed / nx);
            let d = cell * (i as f64 - si as f64).hypot(j as f64 - sj as f64);
            let z = root3 * d / length;
            let decay = (1.0 + z) * (-z).exp();
            let [w, sx, sy] = grid[seed];
            let v = [sx / w * decay, sy / w * decay];
            max_magnitude = max_magnitude.max(v[0].hypot(v[1]));
            values[k] = v;
        }
    }
    ResidualCorrection {
        origin,
        cell,
        nx,
        ny,
        values,
        length,
        max_magnitude,
        rows: rows.len(),
        model,
    }
}

/// Separable Gaussian blur (sd `sigma` cells, truncated at 3 sd) of every channel.
fn blur(grid: &mut [[f64; 3]], nx: usize, ny: usize, sigma: f64) {
    let radius = (3.0 * sigma).ceil() as isize;
    let weights: Vec<f64> = (-radius..=radius)
        .map(|k| (-0.5 * (k as f64 / sigma).powi(2)).exp())
        .collect();
    let mut out = vec![[0.0; 3]; grid.len()];
    for pass in 0..2 {
        let (outer, inner) = if pass == 0 { (ny, nx) } else { (nx, ny) };
        let at = |o: usize, i: usize| if pass == 0 { o * nx + i } else { i * nx + o };
        for o in 0..outer {
            for i in 0..inner {
                let mut acc = [0.0; 3];
                for (w, k) in weights.iter().zip(-radius..=radius) {
                    let src = i as isize + k;
                    if src < 0 || src >= inner as isize {
                        continue;
                    }
                    let v = grid[at(o, src as usize)];
                    for c in 0..3 {
                        acc[c] += w * v[c];
                    }
                }
                out[at(o, i)] = acc;
            }
        }
        grid.copy_from_slice(&out);
    }
}

/// The nearest `seed` cell of every cell (two raster passes propagating the neighbours' nearest
/// seeds, compared by Euclidean distance: exact up to rare raster ties); `None` without seeds.
fn nearest_seed(seed: &[bool], nx: usize, ny: usize) -> Vec<Option<usize>> {
    let mut nearest: Vec<Option<usize>> = (0..seed.len()).map(|k| seed[k].then_some(k)).collect();
    let distance2 = |k: usize, s: usize| {
        let (di, dj) = (
            (k % nx) as f64 - (s % nx) as f64,
            (k / nx) as f64 - (s / nx) as f64,
        );
        di * di + dj * dj
    };
    let visit = |k: usize, neighbours: &[Option<usize>], nearest: &mut Vec<Option<usize>>| {
        for n in neighbours.iter().flatten() {
            if let Some(s) = nearest[*n] {
                if nearest[k].is_none_or(|t| distance2(k, s) < distance2(k, t)) {
                    nearest[k] = Some(s);
                }
            }
        }
    };
    for j in 0..ny {
        for i in 0..nx {
            let k = j * nx + i;
            let up = j.checked_sub(1).map(|u| u * nx + i);
            let neighbours = [
                i.checked_sub(1).map(|l| j * nx + l),
                up,
                up.and_then(|u| (i > 0).then(|| u - 1)),
                up.and_then(|u| (i + 1 < nx).then_some(u + 1)),
            ];
            visit(k, &neighbours, &mut nearest);
        }
        for i in (0..nx).rev() {
            let k = j * nx + i;
            visit(k, &[(i + 1 < nx).then_some(k + 1)], &mut nearest);
        }
    }
    for j in (0..ny).rev() {
        for i in (0..nx).rev() {
            let k = j * nx + i;
            let down = (j + 1 < ny).then(|| (j + 1) * nx + i);
            let neighbours = [
                (i + 1 < nx).then_some(k + 1),
                down,
                down.and_then(|d| (i + 1 < nx).then_some(d + 1)),
                down.and_then(|d| (i > 0).then(|| d - 1)),
            ];
            visit(k, &neighbours, &mut nearest);
        }
        for i in 0..nx {
            let k = j * nx + i;
            visit(k, &[i.checked_sub(1).map(|l| j * nx + l)], &mut nearest);
        }
    }
    nearest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::PlanetMap;
    use crate::protocol::Point;
    use crate::simulation::{ShotObservation, simulate_sampled_path};
    use nalgebra::{DMatrix, DVector};

    fn matern(d: f64, sd: f64, length: f64) -> f64 {
        let z = 3f64.sqrt() * d.abs() / length;
        sd * sd * (1.0 + z) * (-z).exp()
    }

    /// The smoother is the exact posterior mean of the process: the dense Gaussian conditional
    /// mean `K_f (K_f + N)^-1 y` under the Matern-3/2 process and the banded rounding floor, with
    /// missing rows left out of the conditioning; and with a high signal-to-noise ratio it
    /// recovers an injected smooth field.
    #[test]
    fn smoother_is_the_dense_conditional_mean_and_recovers_a_smooth_field() {
        let n = 60;
        let arc: Vec<f64> = (0..n)
            .map(|t| 4.0 * t as f64 + 0.01 * (t * t) as f64)
            .collect();
        let (sd, length) = (2e-3, 60.0);
        let shape = vec![(sd, length); n];
        let ticks: Vec<f64> = (0..n + 2).map(|k| 1e-10 * (1.0 + (k % 3) as f64)).collect();
        let white: Vec<f64> = (0..n).map(|t| 1e-9 * (1.0 + (t % 5) as f64)).collect();
        let missing: Vec<bool> = (0..n).map(|t| (20..26).contains(&t)).collect();
        let truth: Vec<f64> = arc.iter().map(|s| 3e-3 * (s / 70.0).sin()).collect();
        let mut state = 3u64;
        let mut noise = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((state >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 1e-4
        };
        let y: Vec<f64> = truth.iter().map(|f| f + noise()).collect();
        let got = smooth(&arc, &shape, &ticks, &white, &missing, &y);

        let observed: Vec<usize> = (0..n).filter(|t| !missing[*t]).collect();
        let floor = |i: usize, j: usize| {
            let coefs = |r: usize| [(r, 1.0), (r + 1, -2.0), (r + 2, 1.0)];
            let mut c = if i == j { white[i] } else { 0.0 };
            for (ti, ci) in coefs(i) {
                for (tj, cj) in coefs(j) {
                    if ti == tj {
                        c += ci * cj * ticks[ti];
                    }
                }
            }
            c
        };
        let m = observed.len();
        let k_yy = DMatrix::from_fn(m, m, |a, b| {
            let (i, j) = (observed[a], observed[b]);
            matern(arc[i] - arc[j], sd, length) + floor(i, j)
        });
        let k_fy = DMatrix::from_fn(n, m, |i, b| matern(arc[i] - arc[observed[b]], sd, length));
        let y_obs = DVector::from_iterator(m, observed.iter().map(|&t| y[t]));
        let expected = k_fy * k_yy.cholesky().unwrap().solve(&y_obs);
        for t in 0..n {
            assert!(
                (got[t] - expected[t]).abs() < 1e-6 * sd,
                "row {t}: {} vs {}",
                got[t],
                expected[t]
            );
        }
        let worst = (0..n)
            .map(|t| (got[t] - truth[t]).abs())
            .fold(0.0, f64::max);
        assert!(worst < 0.1 * 3e-3, "worst error {worst}");
    }

    fn rows_on_line(mean: [f64; 2]) -> Vec<Row> {
        (0..200)
            .map(|i| Row {
                center: [400.0 + 5.0 * i as f64, 500.0],
                mean,
                length: 200.0,
            })
            .collect()
    }

    /// The correction equals the row values on the observed rows, decays with the Matern-3/2
    /// correlation of the distance at the process length, and is zero inside a planet disk.
    #[test]
    fn correction_is_strongest_on_observed_rows_and_decays_with_distance() {
        let model = ResidualModel {
            scale: 1.0,
            length: 600.0,
            white: 0.0,
        };
        let planet = Planet {
            x: 900.0,
            y: 800.0,
            radius: 30.0,
            mass: 1000.0,
        };
        let c = spread(
            &rows_on_line([1e-2, -5e-3]),
            200.0,
            &[planet.clone()],
            model,
        );
        let on = c.at(Vec2::new(700.0, 500.0));
        assert!(
            (on.x - 1e-2).abs() < 1e-4 && (on.y + 5e-3).abs() < 1e-4,
            "{on:?}"
        );
        let mut previous = on.x;
        for d in [50.0, 100.0, 200.0, 400.0] {
            let v = c.at(Vec2::new(700.0, 500.0 + d)).x;
            let z = 3f64.sqrt() * d / 200.0;
            let expected = 1e-2 * (1.0 + z) * (-z).exp();
            assert!(v < previous, "not decaying at {d}");
            assert!((v - expected).abs() < 0.15 * 1e-2, "{d}: {v} vs {expected}");
            previous = v;
        }
        assert!(c.at(Vec2::new(700.0, 1500.0)).x.abs() < 1e-2 * 0.01);
        let inside = c.at(Vec2::new(planet.x + 10.0, planet.y));
        assert_eq!((inside.x, inside.y), (0.0, 0.0));
    }

    /// A path past a planet the map lacks: the correction on the path points along the missing
    /// planet's field, and is skipped (no compute) under a negligible model-error scale.
    #[test]
    fn missing_planet_gives_a_correction_toward_it_and_a_small_scale_gates_it() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 800.0,
                y: 650.0,
                radius: 25.0,
                mass: 0.1 * 25f64.powi(3),
            }],
        };
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 6.0,
            points: vec![Point { x: 300.0, y: 500.0 }],
        };
        let points = simulate_sampled_path(&truth, &shot, WorldParameters::default(), 150).unwrap();
        let observed = ShotObservation { points, ..shot };
        let empty = PlanetMap { planets: vec![] };
        let model = ResidualModel {
            scale: 0.5,
            length: 600.0,
            white: 1e-6,
        };
        let (_, built) =
            crate::estimator::residual_correction(&empty, &[observed.clone()], Some(model));
        let correction = built.unwrap();
        let p = Vec2::new(
            f64::from(observed.points[60].x),
            f64::from(observed.points[60].y),
        );
        let field = truth.acceleration(p).unwrap();
        let got = correction.at(p);
        let cosine =
            (got.x * field.x + got.y * field.y) / (got.x.hypot(got.y) * field.x.hypot(field.y));
        assert!(cosine > 0.9, "cosine {cosine}: {got:?} vs {field:?}");
        let small = ResidualModel {
            scale: 1e-6,
            ..model
        };
        // The targeting simulation on the empty map with the correction follows the observed
        // path much more closely than without it.
        let error = |correction: Option<&ResidualCorrection>| {
            let path = crate::simulation::simulate_corrected_targeting_shot(
                &empty,
                correction,
                &observed,
                WorldParameters::default(),
                120,
                &[],
                0.0,
            )
            .unwrap();
            let (a, b) = (&path.points[119], &observed.points[119]);
            f64::from(a.x - b.x).hypot(f64::from(a.y - b.y))
        };
        let (plain, corrected) = (error(None), error(Some(&correction)));
        assert!(corrected < 0.2 * plain, "{corrected} vs {plain}");
        let (_, built) = crate::estimator::residual_correction(&empty, &[observed], Some(small));
        assert_eq!(built.unwrap_err(), ResidualGate::ScaleBelowThreshold);
    }

    /// The estimator publishes the correction with its belief: the model scalars always, and
    /// either the correction or the reason it is absent.
    #[test]
    fn the_estimator_publishes_the_correction_or_its_reason() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 800.0,
                y: 650.0,
                radius: 25.0,
                mass: 0.1 * 25f64.powi(3),
            }],
        };
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 6.0,
            points: vec![Point { x: 300.0, y: 500.0 }],
        };
        let points = simulate_sampled_path(&truth, &shot, WorldParameters::default(), 150).unwrap();
        let mut estimator = crate::estimator::Estimator::default();
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let event = crate::protocol::ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 6.0,
            points,
        };
        let belief = estimator.observe(&event, &cancelled).unwrap();
        assert!(belief.residual_model.is_some());
        assert_eq!(
            belief.residual_correction.is_some(),
            belief.residual_correction_unavailable_reason.is_none()
        );
        assert_ne!(
            belief.residual_correction_unavailable_reason,
            Some(ResidualGate::NotComputed)
        );
    }

    /// An estimator-free belief records the correction as not computed, never as zero.
    #[test]
    fn a_belief_without_estimator_records_the_correction_as_not_computed() {
        let belief = crate::strategy::MapBelief::new(PlanetMap { planets: vec![] }, None);
        assert_eq!(belief.residual_correction_at(Vec2::new(1.0, 1.0)), None);
        let json = belief.diagnostic_json();
        assert_eq!(
            json["residual_correction_unavailable_reason"],
            "not_computed"
        );
        assert!(json["residual_correction"].is_null());
        assert!(json["residual_model"].is_null());
    }
}
