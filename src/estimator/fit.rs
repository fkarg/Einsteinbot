//! One-planet (K=1) trajectory fit and its approximate evidence.
//!
//! Model: a single planet `(x, y, radius)` with mass tied to radius (`MASS_PER_RADIUS_CUBED`, as
//! the local NewtonWars generator does), fitted by multi-start Levenberg-Marquardt to the regular
//! samples of the completed trajectories. The fit is continuous; no grid is searched.
//!
//! Residuals are in units of an explicit noise floor. They are not calibrated observation
//! probabilities: `NOISE_SIGMA` and `COORDS_PER_PATH` are heuristics that set how much a path
//! counts, and the evidence built from them is an approximate ranking, not a posterior.
use super::{MAX_MODES, Shot};
use crate::map::PlanetMap;
use crate::protocol::{Planet, Point};
use crate::simulation::{WorldParameters, reconstruct_acceleration, simulate_sampled_positions};
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt, TerminationReason};
use nalgebra::{DMatrix, DVector, Dyn, Matrix3, SymmetricEigen, U1, VecStorage, Vector3};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Position noise floor in world units (also absorbs small simulator/engine mismatch). It is also
/// the scale of the interior-sample residuals (a regular sample inside the disk), so one number
/// sets how much any positional mismatch counts. Heuristic: sub-unit deflections are not trusted
/// as evidence for a planet.
const NOISE_SIGMA: f64 = 0.5;
/// A path's residual is smooth and strongly correlated along time, so it carries few independent
/// coordinates; each path is weighted as if it had this many (mean squared residual times this).
/// Heuristic. It also stops long or chaotic paths dominating by sample count.
const COORDS_PER_PATH: f64 = 8.0;
const MIN_CONTACT_SIGMA: f64 = 0.25;
/// A contact constrains the planet only near its surface: full weight within `CONTACT_FULL`,
/// tapering smoothly to nothing at `CONTACT_GATE`. Farther contacts stay unexplained, so a
/// contact cannot drag the fit. Heuristic: the gate exceeds the trajectory-only position
/// uncertainty of a hit (about 8 units along the approach in the test fixture), because a
/// narrower gate let a fit slide to where the contact no longer constrains it; the v1 estimator's
/// 8-unit snap was too tight for this.
const CONTACT_GATE: f64 = 40.0;
/// A contact within this distance of a surface is fully constrained by it.
pub(super) const CONTACT_FULL: f64 = 15.0;
/// Radius range of the local generator (an assumption about the server, not protocol fact).
const RADIUS_RANGE: (f64, f64) = (20.0, 40.0);
/// Minimum reconstructed acceleration (units/tick^2) worth proposing a planet for; below it the
/// second differences of f32 positions are mostly rounding.
const ACCELERATION_FLOOR: f64 = 3e-3;
/// Mass is tied to the radius cubed, as the local generator does: `mass = 0.1 * radius^3`.
const MASS_PER_RADIUS_CUBED: f64 = 0.1;
/// Modes closer than this to a bound (in the units of the bound) are saturated: their covariance
/// is not reported.
const BOUND_MARGIN: f64 = 0.5;
/// The correlation matrix of the sensitivities must have its smallest eigenvalue above this, or
/// the parameters are treated as under-identified.
const MIN_CORRELATION_EIGENVALUE: f64 = 1e-4;
const MAX_STARTS: usize = 18;
const LM_PATIENCE: usize = 40;
/// Modes closer than this in centre and radius are the same mode.
const SAME_MODE: (f64, f64) = (10.0, 3.0);

#[derive(Clone, Copy)]
pub(super) struct Problem<'a> {
    pub shots: &'a [Shot],
    pub world: WorldParameters,
    pub cancelled: &'a AtomicBool,
    /// Whether surface-contact terms enter the residual vector.
    pub use_contacts: bool,
}

/// One hypothesis about the world: the flat model (`planet` is `None`) or one planet.
#[derive(Clone, Debug)]
pub(super) struct Mode {
    pub planet: Option<Planet>,
    /// Approximate log evidence including the planet prior; the flat model's is `-0.5 * chi2`.
    pub log_evidence: f64,
    /// `[x, y, radius]` covariance of the planet, `None` when the data leave the parameters
    /// under-identified or the mode is saturated at a bound.
    pub covariance: Option<Matrix3<f64>>,
}

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

/// Covariance of `[x, y, radius]` expressed for `[x, y, mass]` (first-order, from the mass tie).
pub(super) fn covariance_xym(covariance: &Matrix3<f64>, radius: f64) -> DMatrix<f64> {
    let mut transform = DMatrix::identity(3, 3);
    transform[(2, 2)] = 3.0 * MASS_PER_RADIUS_CUBED * radius * radius;
    &transform * DMatrix::from_fn(3, 3, |i, j| covariance[(i, j)]) * transform.transpose()
}

fn surface_error(planet: &Planet, point: &Point) -> f64 {
    (f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y) - planet.radius
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

impl Problem<'_> {
    fn usable(shot: &Shot) -> Option<usize> {
        let regular = shot.obs.points.len().saturating_sub(1);
        (regular >= 2).then_some(regular)
    }

    pub fn path_count(&self) -> usize {
        self.shots
            .iter()
            .filter(|shot| Self::usable(shot).is_some())
            .count()
    }

    /// Normalized trajectory residuals of `map`, one entry per coordinate of each regular sample.
    fn trajectory(&self, map: &PlanetMap, out: &mut Vec<f64>) -> Option<()> {
        for shot in self.shots {
            let Some(regular) = Self::usable(shot) else {
                continue;
            };
            if self.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let predicted = simulate_sampled_positions(map, &shot.obs, self.world, regular).ok()?;
            let scale = (COORDS_PER_PATH / (2 * (regular - 1)) as f64).sqrt() / NOISE_SIGMA;
            for (observed, predicted) in shot.obs.points[1..regular].iter().zip(&predicted[1..]) {
                out.push((f64::from(observed.x) - predicted.x) * scale);
                out.push((f64::from(observed.y) - predicted.y) * scale);
            }
        }
        Some(())
    }

    /// Chi-square of the flat (K=0) model on the trajectories alone.
    pub fn flat_chi2(&self) -> Option<f64> {
        let mut residuals = Vec::new();
        self.trajectory(&PlanetMap { planets: vec![] }, &mut residuals)?;
        let chi2 = residuals.iter().map(|r| r * r).sum::<f64>();
        chi2.is_finite().then_some(chi2)
    }

    /// Residuals of the one-planet model `theta = [x, y, radius]`.
    pub fn planet_residuals(&self, theta: [f64; 3]) -> Option<DVector<f64>> {
        let planet = planet_at(theta);
        let map = PlanetMap {
            planets: vec![planet.clone()],
        };
        let mut residuals = Vec::new();
        self.trajectory(&map, &mut residuals)?;
        for shot in self.shots {
            let Some(regular) = Self::usable(shot) else {
                continue;
            };
            // A regular sample inside the disk is impossible. One entry per sample keeps the
            // residual length independent of the parameters.
            for sample in &shot.obs.points[1..regular] {
                residuals.push((-surface_error(&planet, sample)).max(0.0) / NOISE_SIGMA);
            }
            if let (true, Some(contact)) = (self.use_contacts, &shot.contact) {
                residuals.push(contact_residual(
                    surface_error(&planet, &contact.point),
                    contact.allowance,
                ));
            }
        }
        residuals
            .iter()
            .all(|r| r.is_finite())
            .then(|| DVector::from_vec(residuals))
    }

    fn chi2(&self, theta: [f64; 3]) -> Option<f64> {
        self.planet_residuals(theta).map(|r| r.norm_squared())
    }

    /// Whether `theta` lies inside the radius range and the battlefield, shrunk by `margin`.
    fn in_bounds(&self, theta: [f64; 3], margin: f64) -> bool {
        let r = theta[2];
        (RADIUS_RANGE.0 + margin..=RADIUS_RANGE.1 - margin).contains(&r)
            && theta[0] >= r + margin
            && theta[0] <= self.world.battlefield_width - r - margin
            && theta[1] >= r + margin
            && theta[1] <= self.world.battlefield_height - r - margin
    }

    /// Logistic coordinates keep the centre in the battlefield and the radius in range.
    fn decode(&self, u: &DVector<f64>) -> [f64; 3] {
        let sigmoid = |v: f64| 1.0 / (1.0 + (-v).exp());
        let radius = RADIUS_RANGE.0 + (RADIUS_RANGE.1 - RADIUS_RANGE.0) * sigmoid(u[2]);
        [
            radius + (self.world.battlefield_width - 2.0 * radius) * sigmoid(u[0]),
            radius + (self.world.battlefield_height - 2.0 * radius) * sigmoid(u[1]),
            radius,
        ]
    }

    fn encode(&self, theta: [f64; 3]) -> DVector<f64> {
        let logit = |v: f64| {
            let v = v.clamp(1e-6, 1.0 - 1e-6);
            (v / (1.0 - v)).ln()
        };
        let r = theta[2];
        DVector::from_vec(vec![
            logit((theta[0] - r) / (self.world.battlefield_width - 2.0 * r)),
            logit((theta[1] - r) / (self.world.battlefield_height - 2.0 * r)),
            logit((r - RADIUS_RANGE.0) / (RADIUS_RANGE.1 - RADIUS_RANGE.0)),
        ])
    }

    /// Local minimum from `start`; `None` if cancelled, the model cannot be evaluated, or the
    /// solver reports a failure. Running out of patience is not a failure: the iterate is still a
    /// valid point and is scored by its evidence like any other.
    pub fn refine(&self, start: [f64; 3]) -> Option<([f64; 3], f64)> {
        let (lm, report) = LevenbergMarquardt::new()
            .with_patience(LM_PATIENCE)
            .minimize(Lm {
                problem: *self,
                u: self.encode(start),
            });
        if !report.termination.was_successful()
            && !matches!(report.termination, TerminationReason::LostPatience)
        {
            return None;
        }
        let theta = self.decode(&lm.u);
        Some((theta, self.chi2(theta)?))
    }

    /// Laplace evidence and covariance at `theta`, from central-difference sensitivities.
    pub fn laplace(&self, theta: [f64; 3], chi2: f64) -> Option<(f64, Option<Matrix3<f64>>)> {
        const STEP: f64 = 0.01;
        let mut columns = Vec::with_capacity(3);
        for axis in 0..3 {
            let (mut plus, mut minus) = (theta, theta);
            plus[axis] += STEP;
            minus[axis] -= STEP;
            columns.push(
                (self.planet_residuals(plus)? - self.planet_residuals(minus)?) / (2.0 * STEP),
            );
        }
        let mut information = Matrix3::zeros();
        for i in 0..3 {
            for j in 0..3 {
                information[(i, j)] = columns[i].dot(&columns[j]);
            }
        }
        // Gaussian prior as wide as a uniform one over the allowed ranges.
        let prior_sd = Vector3::new(
            self.world.battlefield_width,
            self.world.battlefield_height,
            RADIUS_RANGE.1 - RADIUS_RANGE.0,
        ) / 12f64.sqrt();
        let scaled =
            Matrix3::from_diagonal(&prior_sd) * information * Matrix3::from_diagonal(&prior_sd);
        let occam = 0.5 * (Matrix3::identity() + scaled).determinant().ln();
        // A planet must beat the flat world by this much on top of the Occam factor: a prior
        // log-odds of 1:20 against a planet.
        let log_evidence = -0.5 * chi2 - occam - (20f64).ln();
        // Report no covariance for a mode saturated at a bound (the Gaussian approximation does
        // not hold there) or with a badly conditioned (diagonal-normalized) information matrix.
        let scale = Matrix3::from_diagonal(&information.diagonal().map(|d| d.sqrt().recip()));
        let correlation = scale * information * scale;
        let well_conditioned = correlation.iter().all(|v| v.is_finite())
            && SymmetricEigen::new(correlation).eigenvalues.min() > MIN_CORRELATION_EIGENVALUE;
        let covariance = (well_conditioned && self.in_bounds(theta, BOUND_MARGIN))
            .then(|| information.try_inverse())
            .flatten()
            .map(|inverse| {
                // Inflate when the residuals exceed what the noise model expects.
                let dof = (COORDS_PER_PATH * self.path_count() as f64 - 3.0).max(1.0);
                inverse * (chi2 / dof).max(1.0)
            });
        log_evidence
            .is_finite()
            .then_some((log_evidence, covariance))
    }
}

struct Lm<'a> {
    problem: Problem<'a>,
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
        self.problem.planet_residuals(self.problem.decode(&self.u))
    }
    fn jacobian(&self) -> Option<DMatrix<f64>> {
        let baseline = self.residuals()?;
        let mut jacobian = DMatrix::zeros(baseline.len(), 3);
        for column in 0..3 {
            let step = 1e-4 * (1.0 + self.u[column].abs());
            let mut shifted = self.u.clone();
            shifted[column] += step;
            let plus = self
                .problem
                .planet_residuals(self.problem.decode(&shifted))?;
            if plus.len() != baseline.len() {
                return None;
            }
            jacobian.set_column(column, &((plus - &baseline) / step));
        }
        Some(jacobian)
    }
}

/// Distinct starting points for the multi-start fit. Warm starts (previous modes) come first,
/// then the proposals that matter: for each path, the peak reconstructed acceleration points
/// toward a planet at a distance set by `|a| = mass / d^2` for a few radii (a birth proposal from
/// curvature, not a grid). Contacts add starts straight ahead of the final motion; they help
/// start the search but never create a planet by themselves.
fn starts(problem: &Problem, warm: &[[f64; 3]]) -> Vec<[f64; 3]> {
    const RADII: [f64; 3] = [20.0, 30.0, 40.0];
    let mut proposals: Vec<[f64; 3]> = warm.to_vec();
    for shot in problem.shots {
        let (Some(contact), points) = (&shot.contact, &shot.obs.points) else {
            continue;
        };
        if let [.., before, last_regular, _] = points.as_slice() {
            let (dx, dy) = (
                f64::from(last_regular.x) - f64::from(before.x),
                f64::from(last_regular.y) - f64::from(before.y),
            );
            let norm = dx.hypot(dy);
            if norm > 0.0 {
                for radius in RADII {
                    proposals.push([
                        f64::from(contact.point.x) + radius * dx / norm,
                        f64::from(contact.point.y) + radius * dy / norm,
                        radius,
                    ]);
                }
            }
        }
    }
    let mut peaks = Vec::new();
    for shot in problem.shots {
        let peak = reconstruct_acceleration(&shot.obs.points)
            .into_iter()
            .map(|(position, acceleration)| {
                (position, acceleration, acceleration.norm_squared().sqrt())
            })
            .max_by(|a, b| a.2.total_cmp(&b.2));
        if let Some((position, acceleration, magnitude)) = peak
            && magnitude > ACCELERATION_FLOOR
        {
            peaks.push((magnitude, position, acceleration));
        }
    }
    peaks.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (magnitude, position, acceleration) in peaks {
        for radius in RADII {
            let distance = (MASS_PER_RADIUS_CUBED * radius.powi(3) / magnitude).sqrt();
            proposals.push([
                position.x + acceleration.x / magnitude * distance,
                position.y + acceleration.y / magnitude * distance,
                radius,
            ]);
        }
    }
    let mut kept: Vec<[f64; 3]> = Vec::new();
    for proposal in proposals {
        if problem.in_bounds(proposal, 0.0)
            && kept.len() < MAX_STARTS
            && !kept.iter().any(|other| same_mode(*other, proposal))
        {
            kept.push(proposal);
        }
    }
    kept
}

pub(super) fn same_mode(a: [f64; 3], b: [f64; 3]) -> bool {
    (a[0] - b[0]).hypot(a[1] - b[1]) < SAME_MODE.0 && (a[2] - b[2]).abs() < SAME_MODE.1
}

/// One-planet modes, at most `MAX_MODES`; `None` when cancelled. They may coincide after
/// refinement; the caller does the final ranking and de-duplication.
/// Every start is first fitted to the trajectories alone, and only the best `MAX_MODES` distinct
/// results are kept (this bounds the costlier work below). When contacts exist the retained
/// modes are refined again with the surface-contact terms, so a contact can sharpen or check a
/// fit but not define it.
pub(super) fn planet_modes(problem: &Problem, warm: &[[f64; 3]]) -> Option<Vec<Mode>> {
    let trajectory_only = Problem {
        use_contacts: false,
        ..*problem
    };
    let starts = starts(problem, warm);
    let next = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(starts.len());
    let mut fits: Vec<([f64; 3], f64)> = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut local = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(start) = starts.get(index) else {
                            break local;
                        };
                        if let Some(fit) = trajectory_only.refine(*start) {
                            local.push(fit);
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        // A panicking worker loses only its own fits; the panic message is still printed.
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    });
    if problem.cancelled.load(Ordering::Relaxed) {
        return None;
    }
    fits.sort_by(|a, b| a.1.total_cmp(&b.1));
    let mut distinct: Vec<([f64; 3], f64)> = Vec::new();
    for fit in fits {
        if distinct.len() < MAX_MODES && !distinct.iter().any(|kept| same_mode(kept.0, fit.0)) {
            distinct.push(fit);
        }
    }
    let has_contacts =
        problem.use_contacts && problem.shots.iter().any(|shot| shot.contact.is_some());
    let mut modes = Vec::new();
    for (theta, chi2) in distinct {
        let refined = if has_contacts {
            // A failed contact refinement keeps the trajectory-only point, rescored with contacts.
            problem
                .refine(theta)
                .or_else(|| problem.chi2(theta).map(|chi2| (theta, chi2)))
        } else {
            Some((theta, chi2))
        };
        if problem.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let Some((theta, chi2)) = refined else {
            continue;
        };
        if let Some((log_evidence, covariance)) = problem.laplace(theta, chi2) {
            modes.push(Mode {
                planet: Some(planet_at(theta)),
                log_evidence,
                covariance,
            });
        }
    }
    Some(modes)
}
