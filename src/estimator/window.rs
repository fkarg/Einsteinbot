//! Windowed field comparison: the estimator's observation model. No dynamics are simulated.
//!
//! The engine scheme (velocity Verlet, `n` substeps per tick) gives exactly
//! `x[m+1] - 2 x[m] + x[m-1] = dt^2 a[m]` for substep positions, so the second difference of the
//! observed tick positions at tick `s` is `dt^2` times the triangle-weighted (`n - |i|`) sum of
//! the substep accelerations around it. Each row compares that observed second difference with
//! the same kernel applied to the model field at positions on the observed path. Between ticks
//! the path is a cubic Hermite interpolant with 4th-order central-difference tangents (the exact
//! launch velocity at tick 0).
//!
//! Noise: the f32 rounding of the observed tick positions (uniform, sd `ulp / sqrt(12)`; its
//! exact covariance, including the correlation between neighbouring rows, is whitened by a
//! banded Cholesky factor) plus an interpolation-error variance per row estimated from the
//! observed ticks alone (`interpolation_error`). The interpolation error dominates near planets:
//! without it the true map has a chi-square 10^3 to 10^5 times its degrees of freedom on a near
//! pass. Rows are one tick wide, which keeps the most information; the variance term, not a
//! longer window, handles near passes (`docs/estimator-evaluation.md`).
//!
//! There is deliberately no launch row. The engine starts a debris shot with a stale
//! acceleration, which corrupts the first tick's relation to the launch velocity but no row built
//! from observed ticks only.
use crate::protocol::Point;
use crate::simulation::WorldParameters;

/// Shorter paths are not used: the interpolation error cannot be estimated from them (it needs
/// 4th differences of the ticks).
const MIN_TICKS: usize = 5;
/// Position error of the Hermite interpolant per unit of the ticks' 4th difference (the error of
/// a cubic Hermite interpolant with exact tangents peaks at 1/384 of the 4th derivative; the
/// tangent estimates and the amplification by the field gradient are folded into this one
/// number). Set so that the true map has a chi-square per coordinate near 1 on synthetic and
/// live paths (`measure_residual_scale_*` in `measurements.rs`).
const INTERPOLATION_SCALE: f64 = 1.0 / 15.0;

/// A planet farther than this from a row's centre, and farther than `FAR_SPEEDS` times the
/// path's distance per tick there, enters the row through a three-point rule matched to the
/// kernel's second and fourth moments instead of all `2n - 1` nodes. Its error stays below 5% of the
/// measurement floor (`far_planets_use_the_three_point_rule` in the tests).
const FAR_DISTANCE: f64 = 60.0;
const FAR_SPEEDS: f64 = 5.0;

type V2 = [f64; 2];

/// Standard deviation of the float32 rounding of a position value (uniform within +-ulp/2).
fn f32_sd(x: f64) -> f64 {
    let a = (x as f32).abs();
    if a == 0.0 || !a.is_finite() {
        return 1e-6;
    }
    (2f64.powi(a.log2().floor() as i32 - 23) / 12f64.sqrt()).max(1e-7)
}

/// Lower-triangular Cholesky factor of a pentadiagonal matrix, by diagonals.
#[derive(Clone, Debug)]
struct Whitener {
    diagonal: Vec<f64>,
    first: Vec<f64>,
    second: Vec<f64>,
}

/// One path's observations prepared for the windowed comparison.
#[derive(Clone, Debug)]
pub(super) struct PathData {
    /// Substep positions through the last regular tick (interpolated between ticks).
    pub nodes: Vec<V2>,
    /// Regular tick positions.
    pub ticks: Vec<V2>,
    /// Row `r` is the second difference at tick `r + 1`; its nodes start at `r * n + 1`.
    kernel: Vec<f64>,
    substeps: usize,
    /// Observed second difference per axis and row.
    y: [Vec<f64>; 2],
    /// Per axis.
    whitener: [Whitener; 2],
    /// First node of each row of a thinned path (`thinned`): kept rows that are neighbours on
    /// the path share nodes. `None` for a full path (row `r` starts at node `r * n + 1`).
    row_node: Option<Vec<usize>>,
    /// Row centres of a thinned path (a full path's are its ticks).
    centers: Option<Vec<V2>>,
    /// Per row: the three-point rule's positions (before, at and after the centre tick) and the
    /// squared distance beyond which a planet uses it.
    quad: Vec<([V2; 3], f64)>,
    /// Weights of the three-point rule's outer and centre positions.
    quad_weights: [f64; 2],
}

impl PathData {
    /// `None` when the path has fewer than `MIN_TICKS` regular ticks. `relative` times the
    /// observed acceleration magnitude of each row is added to that row's noise standard deviation
    /// (in quadrature, both axes), on top of the f32 rounding noise; 0 is the exact noise model.
    /// The relative term widens the likelihood for the search stages (E9 in
    /// `docs/estimator-experiments.md`).
    pub fn new(
        points: &[Point],
        angle_degrees: f64,
        velocity: f64,
        world: WorldParameters,
        relative: f64,
    ) -> Option<Self> {
        // The engine records a debris shot's first dot without stepping it when the shot was
        // created after its slot was simulated that tick: the launch dot is then duplicated.
        let points = match points {
            [first, second, ..] if first == second => &points[1..],
            _ => points,
        };
        let t = points.len().checked_sub(1).filter(|t| *t >= MIN_TICKS)?;
        let n = world.substeps_per_tick;
        let ticks: Vec<V2> = points[..t]
            .iter()
            .map(|p| [f64::from(p.x), f64::from(p.y)])
            .collect();
        let angle = angle_degrees.to_radians();
        let v0 = [velocity * angle.cos(), -velocity * angle.sin()];
        let nodes = interpolate(&ticks, v0, n);
        let dt = 1.0 / n as f64;
        let kernel: Vec<f64> = (0..2 * n - 1)
            .map(|i| (n - (i + 1).abs_diff(n)) as f64 * dt * dt)
            .collect();
        let rows = t - 2;
        // The symmetric rule w1 f(-h) + w0 f(0) + w1 f(h) (h in ticks) matching the kernel's
        // weight, second and fourth moments.
        let moment = |k: i32| -> f64 {
            kernel
                .iter()
                .enumerate()
                .map(|(i, w)| w * ((i as f64 + 1.0 - n as f64) / n as f64).powi(k))
                .sum()
        };
        let (m2, m4) = (moment(2), moment(4));
        let h = (m4 / m2).sqrt();
        let outer = m2 * m2 / (2.0 * m4);
        let quad_weights = [outer, moment(0) - 2.0 * outer];
        let quad = (0..rows)
            .map(|r| {
                let s = r + 1;
                let points = [
                    hermite(&ticks, v0, s - 1, 1.0 - h),
                    ticks[s],
                    hermite(&ticks, v0, s, h),
                ];
                let speed = (ticks[s + 1][0] - ticks[s - 1][0])
                    .hypot(ticks[s + 1][1] - ticks[s - 1][1])
                    / 2.0;
                (points, FAR_DISTANCE.max(FAR_SPEEDS * speed).powi(2))
            })
            .collect();
        let y = [0, 1].map(|axis| {
            (0..rows)
                .map(|r| ticks[r + 2][axis] - 2.0 * ticks[r + 1][axis] + ticks[r][axis])
                .collect::<Vec<f64>>()
        });

        // Interpolation-error variance of each row: the row's kernel weight on each tick interval
        // times that interval's estimated field error. Node `m` lies in interval `(m - 1) / n`.
        let interval_error = interpolation_error(&ticks);
        let interpolation_variance: Vec<f64> = (0..rows)
            .map(|r| {
                let mut error = 0.0;
                for (i, w) in kernel.iter().enumerate() {
                    error += w * interval_error[(r * n + i) / n];
                }
                error * error
            })
            .collect();
        let whitener = [0, 1].map(|axis| {
            // Row `r` has noise coefficients (1, -2, 1) on ticks r, r + 1, r + 2.
            let var = |tick: usize| f32_sd(ticks[tick][axis]).powi(2);
            let mut factor = Whitener {
                diagonal: Vec::with_capacity(rows),
                first: Vec::with_capacity(rows),
                second: Vec::with_capacity(rows),
            };
            for i in 0..rows {
                let relative_sd = relative * y[0][i].hypot(y[1][i]);
                let c0 = var(i)
                    + 4.0 * var(i + 1)
                    + var(i + 2)
                    + interpolation_variance[i]
                    + relative_sd * relative_sd;
                let c1 = if i >= 1 {
                    -2.0 * (var(i) + var(i + 1))
                } else {
                    0.0
                };
                let c2 = if i >= 2 { var(i) } else { 0.0 };
                // Cholesky by diagonals: L[i][i-2], L[i][i-1], L[i][i].
                let l2 = if i >= 2 {
                    c2 / factor.diagonal[i - 2]
                } else {
                    0.0
                };
                let l1 = if i >= 1 {
                    (c1 - l2 * factor.first[i - 1]) / factor.diagonal[i - 1]
                } else {
                    0.0
                };
                factor.second.push(l2);
                factor.first.push(l1);
                factor.diagonal.push((c0 - l1 * l1 - l2 * l2).sqrt());
            }
            factor
        });
        Some(Self {
            nodes,
            ticks,
            kernel,
            substeps: n,
            y,
            whitener,
            row_node: None,
            centers: None,
            quad,
            quad_weights,
        })
    }

    #[cfg_attr(not(test), allow(dead_code))]
    /// Observed acceleration magnitude (second difference) of row `r`.
    pub fn strength(&self, r: usize) -> f64 {
        self.y[0][r].hypot(self.y[1][r])
    }

    /// A path of only the rows `keep` (ascending), each with its own `2n - 1` nodes. The row
    /// noise is the original's marginal variance (correlation between neighbouring rows is
    /// dropped), so this is an approximate objective for the search stage only.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn thinned(&self, keep: &[usize]) -> Option<Self> {
        if keep.is_empty() || self.row_node.is_some() {
            return None;
        }
        let n = self.substeps;
        let mut nodes: Vec<V2> = Vec::new();
        let mut row_node = Vec::with_capacity(keep.len());
        let mut previous: Option<usize> = None;
        for &r in keep {
            match previous {
                // Neighbouring row: its nodes continue the previous row's.
                Some(p) if r == p + 1 => {
                    let start = row_node[row_node.len() - 1] + n;
                    nodes.extend_from_slice(&self.nodes[r * n + n..r * n + 2 * n]);
                    row_node.push(start);
                }
                _ => {
                    row_node.push(nodes.len());
                    nodes.extend_from_slice(&self.nodes[r * n + 1..r * n + 2 * n]);
                }
            }
            previous = Some(r);
        }
        let whitener = [0, 1].map(|axis| {
            let w = &self.whitener[axis];
            Whitener {
                diagonal: keep
                    .iter()
                    .map(|&r| {
                        (w.diagonal[r].powi(2) + w.first[r].powi(2) + w.second[r].powi(2)).sqrt()
                    })
                    .collect(),
                first: vec![0.0; keep.len()],
                second: vec![0.0; keep.len()],
            }
        });
        Some(Self {
            nodes,
            ticks: self.ticks.clone(),
            kernel: self.kernel.clone(),
            substeps: n,
            y: [0, 1].map(|axis| keep.iter().map(|&r| self.y[axis][r]).collect()),
            whitener,
            row_node: Some(row_node),
            centers: Some(keep.iter().map(|&r| self.ticks[r + 1]).collect()),
            quad: keep.iter().map(|&r| self.quad[r]).collect(),
            quad_weights: self.quad_weights,
        })
    }

    fn first_node(&self, r: usize) -> usize {
        self.row_node
            .as_ref()
            .map_or(r * self.substeps + 1, |v| v[r])
    }

    pub fn row_count(&self) -> usize {
        self.y[0].len()
    }

    /// Marginal measurement-noise variance (f32 rounding, interpolation and any relative term) of
    /// row `r` on `axis`.
    #[cfg(test)]
    pub fn floor_variance(&self, axis: usize, r: usize) -> f64 {
        let w = &self.whitener[axis];
        w.diagonal[r].powi(2) + w.first[r].powi(2) + w.second[r].powi(2)
    }

    /// Position on the path at the centre of row `r`.
    pub fn center(&self, r: usize) -> V2 {
        self.centers.as_ref().map_or(self.ticks[r + 1], |c| c[r])
    }

    /// `out[r] = sum_i kernel_i * field[node r * n + 1 + i]` for the rows `rows` (others are left
    /// alone) of one axis of the field at the nodes.
    #[cfg(test)]
    pub fn apply(&self, field: &[f64], out: &mut [f64], rows: std::ops::Range<usize>) {
        for r in rows {
            let first = self.first_node(r);
            out[r] = self
                .kernel
                .iter()
                .zip(&field[first..])
                .map(|(w, f)| w * f)
                .sum();
        }
    }

    /// Add one planet's kernel-weighted field to every row's prediction `rows` (per axis): all
    /// `2n - 1` nodes of a near row, the three-point rule otherwise.
    pub fn add_planet(&self, x: f64, y: f64, mass: f64, rows: &mut [Vec<f64>; 2]) {
        self.add_planet_terms(
            x,
            y,
            |p| {
                let (dx, dy) = (x - p[0], y - p[1]);
                let d = dx.hypot(dy).max(1e-9);
                let magnitude = mass / (d * d);
                [dx / d * magnitude, dy / d * magnitude]
            },
            rows,
        );
    }

    /// Add the kernel-weighted derivatives of one planet's field with respect to its
    /// `[x, y, radius]` (`theta`), indexed `parameter * 2 + axis`, to `rows`.
    pub fn add_planet_derivatives(
        &self,
        theta: [f64; 3],
        mass: f64,
        dmass: f64,
        rows: &mut [Vec<f64>; 6],
    ) {
        self.add_planet_terms(
            theta[0],
            theta[1],
            |p| {
                let (dx, dy) = (theta[0] - p[0], theta[1] - p[1]);
                let d2 = (dx * dx + dy * dy).max(1e-18);
                let inverse_r3 = 1.0 / (d2 * d2.sqrt());
                let inverse_r5 = inverse_r3 / d2;
                let cross = -3.0 * mass * dx * dy * inverse_r5;
                [
                    mass * (inverse_r3 - 3.0 * dx * dx * inverse_r5),
                    cross,
                    cross,
                    mass * (inverse_r3 - 3.0 * dy * dy * inverse_r5),
                    dmass * dx * inverse_r3,
                    dmass * dy * inverse_r3,
                ]
            },
            rows,
        );
    }

    fn add_planet_terms<const N: usize>(
        &self,
        x: f64,
        y: f64,
        term: impl Fn(V2) -> [f64; N],
        rows: &mut [Vec<f64>; N],
    ) {
        let [outer, centre] = self.quad_weights;
        for (r, (points, far2)) in self.quad.iter().enumerate() {
            let (dx, dy) = (x - points[1][0], y - points[1][1]);
            let mut sum = [0.0; N];
            if dx * dx + dy * dy > *far2 {
                let (a, b, c) = (term(points[0]), term(points[1]), term(points[2]));
                for k in 0..N {
                    sum[k] = outer * (a[k] + c[k]) + centre * b[k];
                }
            } else {
                let first = self.first_node(r);
                for (w, p) in self.kernel.iter().zip(&self.nodes[first..]) {
                    let v = term(*p);
                    for k in 0..N {
                        sum[k] += w * v[k];
                    }
                }
            }
            for k in 0..N {
                rows[k][r] += sum[k];
            }
        }
    }

    /// Whitened residual `L^-1 (y - prediction)` of one axis, from per-row predictions.
    pub fn residual(&self, axis: usize, prediction: &[f64], out: &mut [f64]) {
        self.raw_residual(axis, prediction, out);
        self.whiten(axis, out, 0);
    }

    /// Unwhitened `y - prediction` of one axis.
    pub fn raw_residual(&self, axis: usize, prediction: &[f64], out: &mut [f64]) {
        for ((o, y), p) in out.iter_mut().zip(&self.y[axis]).zip(prediction) {
            *o = y - p;
        }
    }

    /// Replace `v` by `L^-1 v`, where `v` is zero before row `start`.
    pub fn whiten(&self, axis: usize, v: &mut [f64], start: usize) {
        let w = &self.whitener[axis];
        for i in start..v.len() {
            let mut s = v[i];
            if i >= 1 {
                s -= w.first[i] * v[i - 1];
            }
            if i >= 2 {
                s -= w.second[i] * v[i - 2];
            }
            v[i] = s / w.diagonal[i];
        }
    }
}

/// Per tick interval, an estimate (acceleration units, from the observed ticks alone) of how far
/// the cubic Hermite nodes can push the model field off: the interval's position error
/// (`INTERPOLATION_SCALE` times the 4th difference of the ticks) times the observed rate of
/// change of the acceleration per unit distance along the path.
fn interpolation_error(ticks: &[V2]) -> Vec<f64> {
    let t = ticks.len();
    let second =
        |s: usize| -> V2 { [0, 1].map(|a| ticks[s + 1][a] - 2.0 * ticks[s][a] + ticks[s - 1][a]) };
    let fourth = |s: usize| -> f64 {
        // Central 4th difference at tick `s`, clamped to the available ticks (t >= MIN_TICKS).
        let s = s.clamp(2, t - 3);
        (0..2)
            .map(|a| {
                (ticks[s + 2][a] - 4.0 * ticks[s + 1][a] + 6.0 * ticks[s][a]
                    - 4.0 * ticks[s - 1][a]
                    + ticks[s - 2][a])
                    .powi(2)
            })
            .sum::<f64>()
            .sqrt()
    };
    (0..t - 1)
        .map(|s| {
            let position_error = INTERPOLATION_SCALE * (fourth(s) + fourth(s + 1)) / 2.0;
            // Rate of change of the observed acceleration across this interval.
            // The end intervals borrow the neighbouring pair of ticks (second differences exist
            // only at ticks 1..=t-2), so no interval gets a zero rate.
            let lo = s.clamp(1, t - 3);
            let (a0, a1) = (second(lo), second(lo + 1));
            let da = (a1[0] - a0[0]).hypot(a1[1] - a0[1]);
            let step = (ticks[s + 1][0] - ticks[s][0]).hypot(ticks[s + 1][1] - ticks[s][1]);
            position_error * da / step.max(1e-9)
        })
        .collect()
}

/// Per-tick velocity estimate: exact launch velocity at tick 0, 4th-order central difference
/// inside, 2nd-order central or one-sided near the ends.
fn tangent(ticks: &[V2], v0: V2, s: usize) -> V2 {
    let t = ticks.len();
    [0, 1].map(|a| {
        let p = |i: usize| ticks[i][a];
        if s == 0 {
            v0[a]
        } else if s == t - 1 {
            (3.0 * p(s) - 4.0 * p(s - 1) + p(s - 2)) / 2.0
        } else if s == 1 || s + 2 > t - 1 {
            (p(s + 1) - p(s - 1)) / 2.0
        } else {
            (-p(s + 2) + 8.0 * p(s + 1) - 8.0 * p(s - 1) + p(s - 2)) / 12.0
        }
    })
}

/// The cubic Hermite interpolant at fraction `u` of tick interval `s`.
fn hermite(ticks: &[V2], v0: V2, s: usize, u: f64) -> V2 {
    let h00 = 2.0 * u.powi(3) - 3.0 * u * u + 1.0;
    let h10 = u.powi(3) - 2.0 * u * u + u;
    let h01 = -2.0 * u.powi(3) + 3.0 * u * u;
    let h11 = u.powi(3) - u * u;
    let (m0, m1) = (tangent(ticks, v0, s), tangent(ticks, v0, s + 1));
    [0, 1].map(|a| h00 * ticks[s][a] + h10 * m0[a] + h01 * ticks[s + 1][a] + h11 * m1[a])
}

/// Cubic Hermite interpolation of the ticks at every substep.
fn interpolate(ticks: &[V2], v0: V2, n: usize) -> Vec<V2> {
    let t = ticks.len();
    (0..=(t - 1) * n)
        .map(|m| {
            let (s, i) = (m / n, m % n);
            if s == t - 1 {
                return ticks[s];
            }
            hermite(ticks, v0, s, i as f64 / n as f64)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tick interval, the first and the last included, gets a nonzero error estimate on a
    /// path whose acceleration changes.
    #[test]
    fn end_intervals_get_interpolation_variance() {
        let ticks: Vec<V2> = (0..12)
            .map(|i| [10.0 * i as f64, 0.05 * (i as f64).powi(4)])
            .collect();
        let error = interpolation_error(&ticks);
        assert_eq!(error.len(), ticks.len() - 1);
        assert!(error.iter().all(|e| *e > 0.0), "{error:?}");
    }

    /// A thinned path (neighbouring kept rows share nodes) reads the same field values as the full
    /// path: its raw residuals are the full path's at the kept rows.
    #[test]
    fn thinned_path_matches_full_rows() {
        let points: Vec<Point> = (0..40)
            .map(|i| Point {
                x: 500.0 + 4.0 * i as f32 + 0.01 * (i * i) as f32,
                y: 300.0 - 3.0 * i as f32 + 0.02 * (i * i * i) as f32 * 0.01,
            })
            .collect();
        let world = WorldParameters::default();
        let full = PathData::new(&points, 0.0, 3.0, world, 0.0).unwrap();
        let keep = [0, 1, 2, 7, 12, 13, 30];
        let thin = full.thinned(&keep).unwrap();
        assert_eq!(thin.row_count(), keep.len());
        let field: [Vec<f64>; 2] = [0, 1].map(|axis| {
            full.nodes
                .iter()
                .map(|p| 0.001 * p[axis] + 0.0003 * p[1 - axis])
                .collect()
        });
        let thin_field: [Vec<f64>; 2] = [0, 1].map(|axis| {
            thin.nodes
                .iter()
                .map(|p| 0.001 * p[axis] + 0.0003 * p[1 - axis])
                .collect()
        });
        for axis in 0..2 {
            let mut a = vec![0.0; full.row_count()];
            let mut b = vec![0.0; thin.row_count()];
            full.apply(&field[axis], &mut a, 0..full.row_count());
            thin.apply(&thin_field[axis], &mut b, 0..thin.row_count());
            for (i, &r) in keep.iter().enumerate() {
                assert!((a[r] - b[i]).abs() < 1e-12, "row {r}: {} vs {}", a[r], b[i]);
            }
        }
        assert!(thin.nodes.len() < keep.len() * (2 * world.substeps_per_tick - 1));
    }

    /// The three-point rule for far planets stays far below the f32 rounding floor of every row
    /// against the full kernel, on simulated paths from slow to the velocity cap, the field and
    /// its parameter derivatives alike (near planets use the full kernel by construction).
    #[test]
    fn far_planets_use_the_three_point_rule() {
        use crate::map::PlanetMap;
        use crate::protocol::Planet;
        use crate::simulation::{ShotObservation, simulate_shot};
        let world = WorldParameters::default();
        let planet = |x: f64, y: f64, radius: f64| Planet {
            x,
            y,
            radius,
            mass: radius.powi(3) / 10.0,
        };
        let planets = vec![
            planet(700.0, 600.0, 40.0),
            planet(1100.0, 900.0, 30.0),
            planet(900.0, 300.0, 20.0),
        ];
        let map = PlanetMap {
            planets: planets.clone(),
        };
        let (mut worst, mut far, mut near): (f64, usize, usize) = (0.0, 0, 0);
        for velocity in [4.0, 8.0, 20.0, 50.0] {
            for angle in (0..360).step_by(45) {
                let angle = angle as f64;
                let shot = ShotObservation {
                    player_id: 1,
                    angle_degrees: angle,
                    velocity,
                    points: vec![Point { x: 900.0, y: 650.0 }],
                };
                let path = simulate_shot(&map, &shot, world, 400).unwrap();
                let Some(data) = PathData::new(&path.points, angle, velocity, world, 0.0) else {
                    continue;
                };
                let rows = data.row_count();
                for (points, far2) in &data.quad {
                    for p in &planets {
                        if (p.x - points[1][0]).powi(2) + (p.y - points[1][1]).powi(2) > *far2 {
                            far += 1;
                        } else {
                            near += 1;
                        }
                    }
                }
                for p in &planets {
                    let mut quick = [vec![0.0; rows], vec![0.0; rows]];
                    data.add_planet(p.x, p.y, p.mass, &mut quick);
                    let theta = [p.x, p.y, p.radius];
                    let mut quick_d: [Vec<f64>; 6] = std::array::from_fn(|_| vec![0.0; rows]);
                    data.add_planet_derivatives(
                        theta,
                        p.mass,
                        3.0 * p.mass / p.radius,
                        &mut quick_d,
                    );
                    for axis in 0..2 {
                        let field: Vec<f64> = data
                            .nodes
                            .iter()
                            .map(|n| {
                                let (dx, dy) = (p.x - n[0], p.y - n[1]);
                                let d = dx.hypot(dy);
                                [dx, dy][axis] / d * p.mass / (d * d)
                            })
                            .collect();
                        let mut exact = vec![0.0; rows];
                        data.apply(&field, &mut exact, 0..rows);
                        for r in 0..rows {
                            let floor = data.floor_variance(axis, r).sqrt();
                            let e = (quick[axis][r] - exact[r]).abs() / floor;
                            worst = worst.max(e);
                        }
                        // The x-derivative of the field, against a central difference of the exact
                        // kernel (scaled to the change a 0.1-unit move makes).
                        let h = 1e-3;
                        let shifted = |dx0: f64| -> Vec<f64> {
                            let f: Vec<f64> = data
                                .nodes
                                .iter()
                                .map(|n| {
                                    let (dx, dy) = (p.x + dx0 - n[0], p.y - n[1]);
                                    let d = dx.hypot(dy);
                                    [dx, dy][axis] / d * p.mass / (d * d)
                                })
                                .collect();
                            let mut out = vec![0.0; rows];
                            data.apply(&f, &mut out, 0..rows);
                            out
                        };
                        let (plus, minus) = (shifted(h), shifted(-h));
                        for r in 0..rows {
                            let numeric = (plus[r] - minus[r]) / (2.0 * h);
                            let floor = data.floor_variance(axis, r).sqrt();
                            let e = 0.1 * (quick_d[axis][r] - numeric).abs() / floor;
                            worst = worst.max(e);
                        }
                    }
                }
            }
        }
        assert!(far > 1000 && near > 10, "far {far} near {near}");
        assert!(worst < 0.05, "worst error {worst} rounding sd");
    }

    /// The pentadiagonal factor reproduces the rounding-noise covariance of the rows computed
    /// independently (the interpolation variance is negligible on this smooth path).
    #[test]
    fn whitening_factor_reproduces_the_noise_covariance() {
        let points: Vec<Point> = (0..30)
            .map(|i| Point {
                x: 1000.0 + 3.0 * i as f32 + 0.001 * (i * i) as f32,
                y: 20.0 - 5.0 * i as f32,
            })
            .collect();
        let path = PathData::new(&points, 0.0, 3.0, WorldParameters::default(), 0.0).unwrap();
        let sd = |tick: usize, axis: usize| {
            let p = &points[tick];
            f32_sd(f64::from(if axis == 0 { p.x } else { p.y }))
        };
        let coefs = |row: usize| [(row, 1.0), (row + 1, -2.0), (row + 2, 1.0)];
        for axis in 0..2 {
            let w = &path.whitener[axis];
            let l = |row: usize, col: usize| match row - col {
                0 => w.diagonal[row],
                1 => w.first[row],
                _ => w.second[row],
            };
            for i in 0..path.row_count() {
                for j in i.saturating_sub(2)..=i {
                    let mut expected = 0.0;
                    for (ti, ci) in coefs(i) {
                        for (tj, cj) in coefs(j) {
                            if ti == tj {
                                expected += ci * cj * sd(ti, axis).powi(2);
                            }
                        }
                    }
                    let got: f64 = (i.saturating_sub(2)..=j)
                        .filter(|k| i - k <= 2 && j - k <= 2)
                        .map(|k| l(i, k) * l(j, k))
                        .sum();
                    assert!(
                        (got - expected).abs() <= 1e-4 * expected.abs() + 1e-14,
                        "axis {axis} rows {i},{j}: {got} vs {expected}"
                    );
                }
            }
        }
    }
}
