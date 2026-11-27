//! Joint fit of K planets `(x, y, radius)` to the windowed field comparison of every path
//! (`window`), and its approximate evidence.
//!
//! Mass is tied to the radius (`MASS_PER_RADIUS_CUBED`, as the local NewtonWars generator does).
//! The residual vector is: the whitened window residuals of every path, one interior-sample
//! penalty per (path, planet) (an observed regular tick inside a disk is impossible), and one
//! gated surface-contact term per contact. Jacobians are analytic. The fit is a continuous
//! Levenberg-Marquardt refinement in logistic coordinates; no grid is searched.
//!
//! Evidence is `-chi2/2 - Occam(Laplace) - K ln 20`, under the noise the shots carry
//! (`noise.rs`; the same for every map compared, so the log-determinant is left out). The
//! weights built from it are heuristic rankings, not calibrated probabilities.
use super::Shot;
use super::window::PathData;
use super::work::{Part, Stats, par_map};
use crate::protocol::{Planet, Point};
use crate::simulation::WorldParameters;
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, SymmetricEigen, U1, VecStorage};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Scale of the interior-sample penalty residual, in world units.
const PENALTY_SIGMA: f64 = 0.5;
const MIN_CONTACT_SIGMA: f64 = 0.25;
/// A contact constrains the planet only near its surface: full weight within `CONTACT_FULL`,
/// tapering smoothly to nothing at `CONTACT_GATE`. Farther contacts stay unexplained, so a
/// contact cannot drag the fit. Heuristic: the gate exceeds the trajectory-only position
/// uncertainty of a hit.
const CONTACT_GATE: f64 = 40.0;
/// A contact within this distance of a surface is fully constrained by it.
pub(super) const CONTACT_FULL: f64 = 15.0;
/// Radius range of the local generator (an assumption about the server, not protocol fact).
const RADIUS_RANGE: (f64, f64) = (20.0, 40.0);
/// Mass is tied to the radius cubed, as the local generator does: `mass = 0.1 * radius^3`.
const MASS_PER_RADIUS_CUBED: f64 = 0.1;
/// Planets closer than this to a bound (in the units of the bound) are saturated: no covariance.
const BOUND_MARGIN: f64 = 0.5;
/// The correlation matrix of the sensitivities must have its smallest eigenvalue above this, or
/// the parameters are treated as under-identified.
const MIN_CORRELATION_EIGENVALUE: f64 = 1e-4;
const LM_PATIENCE: usize = 40;
/// An evaluation with fewer planet-times-node terms than this is not split over threads.
const PARALLEL_MIN_PLANET_NODES: usize = 40_000;
/// More threads than this per evaluation do not help (measured: 8 and 16 threads are alike).
const MAX_EVALUATION_THREADS: usize = 8;
/// Prior odds against each additional planet. Not a count prior: a Poisson(24) count prior with
/// the `K!` label factor adds `+ln 24` per planet, which makes a planet the data do not constrain
/// (Occam term near 0) free, and the flat-world test grows five such planets in a corner
/// (P13, `docs/estimator-experiments.md`); a planet must be born from trajectory evidence.
const PLANET_PRIOR_ODDS: f64 = 20.0;

pub(super) fn planet_at(theta: [f64; 3]) -> Planet {
    Planet {
        x: theta[0],
        y: theta[1],
        radius: theta[2],
        mass: MASS_PER_RADIUS_CUBED * theta[2].powi(3),
    }
}

pub(super) fn theta_of(planet: &Planet) -> [f64; 3] {
    [planet.x, planet.y, planet.radius]
}

/// Covariance of `[x, y, radius]` per planet expressed for `[x, y, mass]` (first-order, from the
/// mass tie).
pub(super) fn covariance_xym(covariance: &DMatrix<f64>, planets: &[Planet]) -> DMatrix<f64> {
    let mut transform = DMatrix::identity(covariance.nrows(), covariance.ncols());
    for (i, planet) in planets.iter().enumerate() {
        transform[(3 * i + 2, 3 * i + 2)] = 3.0 * MASS_PER_RADIUS_CUBED * planet.radius.powi(2);
    }
    &transform * covariance * transform.transpose()
}

fn surface_error(theta: &[f64; 3], point: &Point) -> f64 {
    (f64::from(point.x) - theta[0]).hypot(f64::from(point.y) - theta[1]) - theta[2]
}

/// Normalized residual of a terminal contact whose signed distance outside the planet surface is
/// `signed`. The projectile stops at the first substep inside the disk, so the true penetration is
/// roughly uniform in `[-allowance, 0]`: centre there, with the matching standard deviation (never
/// below `MIN_CONTACT_SIGMA`). The pull tapers to nothing between `CONTACT_FULL` and
/// `CONTACT_GATE`, so a disagreeing contact is left unexplained instead of dragging the fit.
pub(super) fn contact_residual(signed: f64, allowance: f64) -> f64 {
    let error = signed + allowance / 2.0;
    let t = ((error.abs() - CONTACT_FULL) / (CONTACT_GATE - CONTACT_FULL)).clamp(0.0, 1.0);
    let sigma = (allowance / 12f64.sqrt()).max(MIN_CONTACT_SIGMA);
    error * (1.0 - t * t * (3.0 - 2.0 * t)) / sigma
}

/// Acceleration at one observed tick that the model leaves unexplained.
pub(super) struct Unexplained {
    pub position: [f64; 2],
    pub acceleration: [f64; 2],
    /// Whitened residual (innovation) magnitude (about 1.25 on average, 1.4 rms, for a correct
    /// model and noise).
    pub significance: f64,
}

pub(super) struct Evidence {
    pub log_evidence: f64,
    /// `[x, y, radius]` covariance of all planets, `None` when the data leave the parameters
    /// under-identified or a planet sits at a bound.
    pub covariance: Option<DMatrix<f64>>,
}

/// Limits of one `refine`: a deterministic evaluation count and an optional wall-clock instant
/// (in addition to the `Fit`'s own deadline).
#[derive(Clone, Copy, Debug)]
pub(super) struct Budget {
    pub evaluations: u64,
    pub deadline: Option<Instant>,
    /// Threads one evaluation may use (1: none are spawned).
    pub threads: usize,
}

impl Budget {
    #[cfg(test)]
    pub const UNLIMITED: Budget = Budget {
        evaluations: u64::MAX,
        deadline: None,
        threads: 1,
    };
}

pub(super) struct Fit<'a> {
    shots: &'a [Shot],
    world: WorldParameters,
    cancelled: &'a AtomicBool,
    /// `refine` stops improving (keeping its best point so far) at this instant.
    deadline: Option<Instant>,
    use_contacts: bool,
    /// Planets held fixed; the fitted planets add to their field. Evidence needs this empty.
    fixed: Vec<Planet>,
    /// Field of the fixed planets at each shot's nodes (empty arrays without a path or planets).
    base: Vec<[Vec<f64>; 2]>,
    stats: Option<&'a Stats>,
}

impl<'a> Fit<'a> {
    pub fn new(
        shots: &'a [Shot],
        world: WorldParameters,
        cancelled: &'a AtomicBool,
        use_contacts: bool,
        fixed: Vec<Planet>,
    ) -> Self {
        let base = shots
            .iter()
            .map(|shot| {
                let mut field = [Vec::new(), Vec::new()];
                if let (Some(path), false) = (&shot.path, fixed.is_empty()) {
                    field = [vec![0.0; path.row_count()], vec![0.0; path.row_count()]];
                    for planet in &fixed {
                        path.add_planet(planet.x, planet.y, planet.mass, &mut field);
                    }
                }
                field
            })
            .collect();
        Self {
            shots,
            world,
            cancelled,
            deadline: None,
            use_contacts,
            fixed,
            base,
            stats: None,
        }
    }

    pub fn with_stats(mut self, stats: &'a Stats) -> Self {
        self.stats = Some(stats);
        self
    }

    pub fn with_deadline(mut self, deadline: Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Predicted second difference of every row of one path (per axis): the fixed planets'
    /// plus `theta`'s kernel-weighted field.
    pub(super) fn field(&self, index: usize, path: &PathData, theta: &[[f64; 3]]) -> [Vec<f64>; 2] {
        let mut field = if self.base[index][0].is_empty() {
            [vec![0.0; path.row_count()], vec![0.0; path.row_count()]]
        } else {
            self.base[index].clone()
        };
        for th in theta {
            let planet = planet_at(*th);
            path.add_planet(planet.x, planet.y, planet.mass, &mut field);
        }
        field
    }

    fn data_rows(&self) -> usize {
        self.shots
            .iter()
            .filter_map(|shot| shot.path.as_ref())
            .map(|path| 2 * path.row_count())
            .sum()
    }

    /// Residual vector (and its Jacobian with respect to `theta`) of the free planets.
    pub(super) fn evaluate(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
        threads: usize,
    ) -> Option<(DVector<f64>, Option<DMatrix<f64>>)> {
        let started = Instant::now();
        let out = self.compute(theta, want_jacobian, threads);
        if let Some(stats) = self.stats {
            let part = if want_jacobian {
                Part::JacobianEval
            } else {
                Part::ResidualEval
            };
            stats.add(part, started.elapsed());
        }
        out
    }

    /// The whitened window residuals of one path, axis by axis.
    fn path_residual(&self, index: usize, theta: &[[f64; 3]]) -> Vec<f64> {
        let path = self.shots[index].path.as_ref().expect("a path");
        let r = path.row_count();
        let field = self.field(index, path, theta);
        let mut res = vec![0.0; 2 * r];
        for axis in 0..2 {
            path.residual(axis, &field[axis], &mut res[axis * r..(axis + 1) * r]);
        }
        res
    }

    /// Jacobian columns `3 * planets.start ..` of the window rows of every path, column-major with
    /// `total` rows (the rows of paths come in `self.shots` order, `starts` locates each path).
    fn data_jacobian(
        &self,
        theta: &[[f64; 3]],
        planets: std::ops::Range<usize>,
        total: usize,
        starts: &[(usize, usize)],
    ) -> Vec<f64> {
        let mut block = vec![0.0; total * 3 * planets.len()];
        for &(index, row) in starts {
            let path = self.shots[index].path.as_ref().expect("a path");
            let r = path.row_count();
            let mut scratch = vec![0.0; r];
            for j in planets.clone() {
                let th = &theta[j];
                let mass = MASS_PER_RADIUS_CUBED * th[2].powi(3);
                let dmass = 3.0 * MASS_PER_RADIUS_CUBED * th[2].powi(2);
                let mut derivative: [Vec<f64>; 6] = std::array::from_fn(|_| vec![0.0; r]);
                path.add_planet_derivatives(*th, mass, dmass, &mut derivative);
                for param in 0..3 {
                    let column = (3 * (j - planets.start) + param) * total;
                    for axis in 0..2 {
                        scratch.copy_from_slice(&derivative[param * 2 + axis]);
                        path.whiten(axis, &mut scratch);
                        for (i, v) in scratch.iter().enumerate() {
                            block[column + row + axis * r + i] = -v;
                        }
                    }
                }
            }
        }
        block
    }

    fn compute(
        &self,
        theta: &[[f64; 3]],
        want_jacobian: bool,
        threads: usize,
    ) -> Option<(DVector<f64>, Option<DMatrix<f64>>)> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let k = theta.len();
        let paths = self.shots.iter().filter(|shot| shot.path.is_some()).count();
        // Threads only pay off when there is enough work to share (measured in `diagnose.rs`).
        let planet_nodes: usize = k * self
            .shots
            .iter()
            .filter_map(|shot| shot.path.as_ref())
            .map(|path| path.nodes.len())
            .sum::<usize>();
        let threads = if planet_nodes >= PARALLEL_MIN_PLANET_NODES {
            threads.min(MAX_EVALUATION_THREADS)
        } else {
            1
        };
        let contacts = if self.use_contacts && k > 0 {
            self.shots
                .iter()
                .filter(|shot| shot.contact.is_some())
                .count()
        } else {
            0
        };
        let data_rows = self.data_rows();
        let total = data_rows + paths * k + contacts;
        // (shot index, first data row) of every path.
        let mut starts = Vec::with_capacity(paths);
        let mut row = 0;
        for (index, shot) in self.shots.iter().enumerate() {
            if let Some(path) = &shot.path {
                starts.push((index, row));
                row += 2 * path.row_count();
            }
        }
        let mut res = Vec::with_capacity(total);
        for part in par_map(&starts, threads.min(paths).min(k.max(1)), |&(index, _)| {
            self.path_residual(index, theta)
        }) {
            res.extend(part);
        }
        res.resize(total, 0.0);
        let mut jacobian = want_jacobian.then(|| {
            let mut jacobian = DMatrix::zeros(total, 3 * k);
            // Planets are split into one chunk per thread; each chunk is a contiguous block of columns.
            let chunk = k.div_ceil(threads.clamp(1, k.max(1)));
            let chunks: Vec<std::ops::Range<usize>> = (0..k)
                .step_by(chunk.max(1))
                .map(|first| first..(first + chunk).min(k))
                .collect();
            let blocks = par_map(&chunks, threads, |planets| {
                self.data_jacobian(theta, planets.clone(), total, &starts)
            });
            for (planets, block) in chunks.iter().zip(blocks) {
                jacobian.as_mut_slice()[3 * planets.start * total..][..block.len()]
                    .copy_from_slice(&block);
            }
            jacobian
        });
        let mut row = data_rows;
        for shot in self.shots.iter().filter(|shot| shot.path.is_some()) {
            let path = shot.path.as_ref().expect("filtered");
            for (j, th) in theta.iter().enumerate() {
                // Closest point of the path (the chords between ticks: at high speed a planet can
                // sit between two ticks).
                let (distance, px, py) = path
                    .ticks
                    .windows(2)
                    .map(|w| {
                        let (a, b) = (w[0], w[1]);
                        let (ex, ey) = (b[0] - a[0], b[1] - a[1]);
                        let t = (((th[0] - a[0]) * ex + (th[1] - a[1]) * ey)
                            / (ex * ex + ey * ey).max(1e-18))
                        .clamp(0.0, 1.0);
                        let (px, py) = (a[0] + t * ex, a[1] + t * ey);
                        ((th[0] - px).hypot(th[1] - py), px, py)
                    })
                    .min_by(|a, b| a.0.total_cmp(&b.0))
                    .expect("paths have ticks");
                let value = (th[2] - distance).max(0.0) / PENALTY_SIGMA;
                res[row] = value;
                if let (Some(jacobian), true) = (jacobian.as_mut(), value > 0.0) {
                    jacobian[(row, 3 * j)] = -(th[0] - px) / distance.max(1e-9) / PENALTY_SIGMA;
                    jacobian[(row, 3 * j + 1)] = -(th[1] - py) / distance.max(1e-9) / PENALTY_SIGMA;
                    jacobian[(row, 3 * j + 2)] = 1.0 / PENALTY_SIGMA;
                }
                row += 1;
            }
        }
        if contacts > 0 {
            for contact in self.shots.iter().filter_map(|shot| shot.contact.as_ref()) {
                // Each contact constrains only the planet whose surface it is closest to, so
                // overlapping planets are never double counted.
                let (j, signed) = theta
                    .iter()
                    .enumerate()
                    .map(|(j, th)| (j, surface_error(th, &contact.point)))
                    .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
                    .expect("k > 0");
                res[row] = contact_residual(signed, contact.allowance);
                if let Some(jacobian) = jacobian.as_mut() {
                    let h = 1e-4;
                    let slope = (contact_residual(signed + h, contact.allowance)
                        - contact_residual(signed - h, contact.allowance))
                        / (2.0 * h);
                    let th = &theta[j];
                    let distance = (signed + th[2]).max(1e-9);
                    jacobian[(row, 3 * j)] =
                        slope * (th[0] - f64::from(contact.point.x)) / distance;
                    jacobian[(row, 3 * j + 1)] =
                        slope * (th[1] - f64::from(contact.point.y)) / distance;
                    jacobian[(row, 3 * j + 2)] = -slope;
                }
                row += 1;
            }
        }
        res.iter()
            .all(|v| v.is_finite())
            .then(|| (DVector::from_vec(res), jacobian))
    }

    /// Chi-square.
    pub fn chi2(&self, theta: &[[f64; 3]]) -> Option<f64> {
        self.evaluate(theta, false, 1)
            .map(|(residuals, _)| residuals.norm_squared())
    }

    /// Whether every planet lies inside the radius range and the battlefield, shrunk by `margin`.
    pub fn in_bounds(&self, theta: &[[f64; 3]], margin: f64) -> bool {
        theta.iter().all(|th| {
            let r = th[2];
            (RADIUS_RANGE.0 + margin..=RADIUS_RANGE.1 - margin).contains(&r)
                && th[0] >= r + margin
                && th[0] <= self.world.battlefield_width - r - margin
                && th[1] >= r + margin
                && th[1] <= self.world.battlefield_height - r - margin
        })
    }

    /// Logistic coordinates keep each centre in the battlefield and each radius in range.
    fn decode(&self, u: &DVector<f64>) -> Vec<[f64; 3]> {
        let sigmoid = |v: f64| 1.0 / (1.0 + (-v).exp());
        u.as_slice()
            .chunks(3)
            .map(|c| {
                let radius = RADIUS_RANGE.0 + (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(c[2]);
                [
                    radius + (self.world.battlefield_width - 2.0 * radius) * sigmoid(c[0]),
                    radius + (self.world.battlefield_height - 2.0 * radius) * sigmoid(c[1]),
                    radius,
                ]
            })
            .collect()
    }

    fn encode(&self, theta: &[[f64; 3]]) -> DVector<f64> {
        let logit = |v: f64| {
            let v = v.clamp(1e-6, 1.0 - 1e-6);
            (v / (1.0 - v)).ln()
        };
        DVector::from_iterator(
            3 * theta.len(),
            theta.iter().flat_map(|th| {
                let r = th[2];
                [
                    logit((th[0] - r) / (self.world.battlefield_width - 2.0 * r)),
                    logit((th[1] - r) / (self.world.battlefield_height - 2.0 * r)),
                    logit((r - RADIUS_RANGE.0) / (RADIUS_RANGE.1 - RADIUS_RANGE.0)),
                ]
            }),
        )
    }

    /// Local minimum from `start`, or the best point reached when the deadline passes (never worse
    /// than `start`: `Lm` remembers the lowest-norm point it evaluated, because the optimizer
    /// leaves its last, possibly worse, trial step as the parameters when it is interrupted).
    /// `None` if cancelled or the model cannot be evaluated at the result. Running out of patience
    /// is not a failure: the iterate is still a valid point and is scored by its evidence like any
    /// other.
    pub fn refine(&self, start: &[[f64; 3]], budget: &Budget) -> Option<(Vec<[f64; 3]>, f64)> {
        let started = Instant::now();
        let used = Cell::new(0);
        let mut u = self.encode(start);
        let (lm, _) = LevenbergMarquardt::new()
            .with_patience(LM_PATIENCE)
            .minimize(Lm {
                fit: self,
                u: u.clone(),
                best: RefCell::new(None),
                budget,
                used: &used,
            });
        if let Some((_, best)) = lm.best.into_inner() {
            u = best;
        }
        let theta = self.decode(&u);
        let chi2 = self.chi2(&theta)?;
        if let Some(stats) = self.stats {
            stats.add(Part::LmSolve, started.elapsed());
        }
        Some((theta, chi2))
    }

    /// Laplace evidence and covariance at `theta` (all planets free, none fixed).
    pub fn evidence(&self, theta: &[[f64; 3]]) -> Option<Evidence> {
        let started = Instant::now();
        let out = self.laplace(theta);
        if let Some(stats) = self.stats {
            stats.add(Part::Evidence, started.elapsed());
        }
        out
    }

    fn laplace(&self, theta: &[[f64; 3]]) -> Option<Evidence> {
        debug_assert!(self.fixed.is_empty());
        let k = theta.len();
        let (residuals, jacobian) = self.evaluate(theta, true, 1)?;
        let chi2 = residuals.norm_squared();
        if k == 0 {
            return Some(Evidence {
                log_evidence: -0.5 * chi2,
                covariance: None,
            });
        }
        let jacobian = jacobian.expect("requested");
        let covariance_started = Instant::now();
        let information = jacobian.tr_mul(&jacobian);
        // Gaussian prior as wide as a uniform one over the allowed ranges.
        let prior_sd = |i: usize| {
            [
                self.world.battlefield_width,
                self.world.battlefield_height,
                RADIUS_RANGE.1 - RADIUS_RANGE.0,
            ][i % 3]
                / 12f64.sqrt()
        };
        let scaled = DMatrix::from_fn(3 * k, 3 * k, |i, j| {
            information[(i, j)] * prior_sd(i) * prior_sd(j) + if i == j { 1.0 } else { 0.0 }
        });
        let occam: f64 = scaled
            .cholesky()
            .map(|c| (0..3 * k).map(|i| c.l()[(i, i)].ln()).sum())?;
        let log_evidence = -0.5 * chi2 - occam - k as f64 * PLANET_PRIOR_ODDS.ln();
        // No covariance for a mode saturated at a bound (the Gaussian approximation does not hold
        // there) or with a badly conditioned (diagonal-normalized) information matrix.
        let scale = DMatrix::from_diagonal(&information.diagonal().map(|d| d.sqrt().recip()));
        let correlation = &scale * &information * &scale;
        let well_conditioned = correlation.iter().all(|v| v.is_finite())
            && SymmetricEigen::new(correlation).eigenvalues.min() > MIN_CORRELATION_EIGENVALUE;
        let covariance = (well_conditioned && self.in_bounds(theta, BOUND_MARGIN))
            .then(|| information.clone().try_inverse())
            .flatten()
            .map(|inverse| {
                // Inflate when the residuals exceed what the noise model expects.
                let dof = (self.data_rows() as f64 - 3.0 * k as f64).max(1.0);
                inverse * (chi2 / dof).max(1.0)
            });
        if let Some(stats) = self.stats {
            stats.add(Part::Covariance, covariance_started.elapsed());
        }
        log_evidence.is_finite().then_some(Evidence {
            log_evidence,
            covariance,
        })
    }

    /// Observed rows ordered by how far the model leaves them unexplained.
    pub fn unexplained(&self, theta: &[[f64; 3]]) -> Vec<Unexplained> {
        let mut out = Vec::new();
        for (index, shot) in self.shots.iter().enumerate() {
            let Some(path) = &shot.path else { continue };
            let r = path.row_count();
            let field = self.field(index, path, theta);
            let (mut raw, mut white) = ([vec![0.0; r], vec![0.0; r]], [vec![0.0; r], vec![0.0; r]]);
            for axis in 0..2 {
                path.raw_residual(axis, &field[axis], &mut raw[axis]);
                white[axis].copy_from_slice(&raw[axis]);
                path.whiten(axis, &mut white[axis]);
            }
            for i in 0..r {
                out.push(Unexplained {
                    position: path.center(i),
                    acceleration: [raw[0][i], raw[1][i]],
                    significance: white[0][i].hypot(white[1][i]),
                });
            }
        }
        out.sort_by(|a, b| b.significance.total_cmp(&a.significance));
        out
    }
}

struct Lm<'a> {
    fit: &'a Fit<'a>,
    u: DVector<f64>,
    /// Lowest residual norm evaluated so far and its parameters.
    best: RefCell<Option<(f64, DVector<f64>)>>,
    budget: &'a Budget,
    /// Evaluations of this refinement so far.
    used: &'a Cell<u64>,
}

impl Lm<'_> {
    /// Whether another evaluation is allowed; counts it when it is.
    fn spend(&self) -> bool {
        let out_of_time = self
            .fit
            .deadline
            .into_iter()
            .chain(self.budget.deadline)
            .any(|d| Instant::now() >= d);
        if out_of_time || self.used.get() >= self.budget.evaluations {
            return false;
        }
        self.used.set(self.used.get() + 1);
        true
    }
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
        if !self.spend() {
            return None;
        }
        let (residuals, _) =
            self.fit
                .evaluate(&self.fit.decode(&self.u), false, self.budget.threads)?;
        let norm = residuals.norm();
        let mut best = self.best.borrow_mut();
        if best.as_ref().is_none_or(|(lowest, _)| norm < *lowest) {
            *best = Some((norm, self.u.clone()));
        }
        Some(residuals)
    }
    fn jacobian(&self) -> Option<DMatrix<f64>> {
        if !self.spend() {
            return None;
        }
        let theta = self.fit.decode(&self.u);
        let (_, jacobian) = self.fit.evaluate(&theta, true, self.budget.threads)?;
        let mut jacobian = jacobian?;
        // Chain to the logistic coordinates: x and y depend on their own coordinate and, through
        // the bounds, on the radius coordinate.
        let sigmoid = |v: f64| 1.0 / (1.0 + (-v).exp());
        for (j, th) in theta.iter().enumerate() {
            let (ux, uy, ur) = (self.u[3 * j], self.u[3 * j + 1], self.u[3 * j + 2]);
            let radius_slope =
                (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(ur) * (1.0 - sigmoid(ur));
            let dx_dux = (self.fit.world.battlefield_width - 2.0 * th[2])
                * sigmoid(ux)
                * (1.0 - sigmoid(ux));
            let dy_duy = (self.fit.world.battlefield_height - 2.0 * th[2])
                * sigmoid(uy)
                * (1.0 - sigmoid(uy));
            let dx_dur = (1.0 - 2.0 * sigmoid(ux)) * radius_slope;
            let dy_dur = (1.0 - 2.0 * sigmoid(uy)) * radius_slope;
            for row in 0..jacobian.nrows() {
                let (jx, jy, jr) = (
                    jacobian[(row, 3 * j)],
                    jacobian[(row, 3 * j + 1)],
                    jacobian[(row, 3 * j + 2)],
                );
                jacobian[(row, 3 * j)] = jx * dx_dux;
                jacobian[(row, 3 * j + 1)] = jy * dy_duy;
                jacobian[(row, 3 * j + 2)] = jx * dx_dur + jy * dy_dur + jr * radius_slope;
            }
        }
        Some(jacobian)
    }
}
