//! Joint physical-planet fitting from regular trajectories and associated surface contacts.
use crate::estimator::CovarianceUnavailableReason;
use crate::map::PlanetMap;
use crate::protocol::{Planet, Point};
use crate::simulation::{ShotObservation, WorldParameters, simulate_sampled_positions};
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, U1, VecStorage, linalg::SVD};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Debug)]
pub struct ContactConstraint {
    pub planet_index: usize,
    pub point: Point,
}

#[derive(Clone, Debug)]
pub struct PlanetFit {
    pub map: PlanetMap,
    /// Local objective covariance in `[center_x, center_y, mass]` order. It does not
    /// represent omitted planets or discrete contact-association uncertainty.
    /// Degrees of freedom count distinct usable paths, not correlated point rows
    /// or inactive inequality rows. This remains a conservative local approximation,
    /// not a calibrated posterior over unknown planet configurations.
    pub covariance: Option<DMatrix<f64>>,
    pub covariance_unavailable_reason: Option<CovarianceUnavailableReason>,
    pub position_rmse: f64,
    pub contact_rmse: f64,
    pub rank: usize,
    pub independent_paths: usize,
    pub evidence_resolution: Option<DMatrix<f64>>,
    pub parameter_to_physical: Option<DMatrix<f64>>,
    pub converged: bool,
}

/// Refine physical planets jointly. Each trajectory contributes its mean robust
/// coordinate loss (pseudo-Huber scale 5 world units), so long paths and missing
/// distant sources cannot dominate merely by supplying many correlated samples.
/// Contacts/free space use 0.5-unit geometric scale; contacts permit up to 0.5
/// units of substep penetration. These scales are a fitting model, not calibrated
/// observation probabilities. A fit worsening contact violation by over 0.5 units
/// is rejected rather than silently moving a known surface to explain missing mass.
pub fn fit_planet_map(
    initial: &PlanetMap,
    observations: &[ShotObservation],
    contacts: &[ContactConstraint],
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Result<PlanetFit, String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("planet fit cancelled".into());
    }
    if initial.planets.is_empty() {
        return Err("planet map is empty".into());
    }
    if !world.battlefield_width.is_finite()
        || !world.battlefield_height.is_finite()
        || world.battlefield_width <= 0.0
        || world.battlefield_height <= 0.0
        || world.substeps_per_tick == 0
    {
        return Err("invalid world dimensions or integration substeps".into());
    }
    for planet in &initial.planets {
        if ![planet.x, planet.y, planet.radius]
            .iter()
            .all(|v| v.is_finite())
            || !(20.0..=40.0).contains(&planet.radius)
            || planet.x < planet.radius
            || planet.y < planet.radius
            || planet.x > world.battlefield_width - planet.radius
            || planet.y > world.battlefield_height - planet.radius
        {
            return Err(
                "initial planets must lie within the battlefield and have radius 20..40".into(),
            );
        }
    }
    if maximum_overlap(initial) > 0.0 {
        return Err("initial planet disks overlap".into());
    }
    if contacts.iter().any(|contact| {
        contact.planet_index >= initial.planets.len()
            || !contact.point.x.is_finite()
            || !contact.point.y.is_finite()
    }) {
        return Err("invalid contact planet index or coordinates".into());
    }
    if observations.iter().any(|shot| {
        !shot.angle_degrees.is_finite()
            || !shot.velocity.is_finite()
            || shot
                .points
                .iter()
                .any(|point| !point.x.is_finite() || !point.y.is_finite())
    }) {
        return Err("trajectory parameters and points must be finite".into());
    }
    // Deterministic replays add no independent field information, even if their
    // protocol player identity differs.
    let mut unique_observations: Vec<ShotObservation> = Vec::new();
    for shot in observations {
        if !unique_observations.iter().any(|old| {
            old.angle_degrees == shot.angle_degrees
                && old.velocity == shot.velocity
                && old.points == shot.points
        }) {
            unique_observations.push(shot.clone());
        }
    }
    let independent_paths = unique_observations
        .iter()
        .filter(|shot| shot.points.len() >= 3)
        .count();
    if independent_paths == 0 && contacts.is_empty() {
        return Err("fit requires trajectory samples or associated contacts".into());
    }
    let seeded = contact_seeded_map(initial, &unique_observations, contacts, world, cancelled);
    let parameters = encode_planets(&seeded, world);
    let problem = Problem {
        parameters,
        observations: unique_observations,
        contacts: contacts.to_vec(),
        world,
        cancelled,
        workers: std::thread::available_parallelism().map_or(1, usize::from),
    };
    let initial_residual = problem
        .residual_vector(&problem.parameters)
        .ok_or("cannot evaluate initial planet map")?;
    if initial_residual.is_empty() {
        return Err("fit requires trajectory samples or associated contacts".into());
    }
    let initial_contact_violation =
        contact_violation(initial, contacts).min(contact_violation(&seeded, contacts));
    let (problem, converged) = if initial_residual.len() >= problem.parameters.len() {
        let (problem, report) = LevenbergMarquardt::new().minimize(problem);
        (problem, report.termination.was_successful())
    } else {
        (problem, false)
    };
    if cancelled.load(Ordering::Relaxed) {
        return Err("planet fit cancelled".into());
    }
    let map = problem
        .decode(&problem.parameters)
        .ok_or("optimizer produced a nonphysical map")?;
    if maximum_overlap(&map) > 0.0 {
        return Err("joint fit produced overlapping planet disks".into());
    }
    if contact_violation(&map, contacts) > initial_contact_violation + 0.5 {
        return Err(
            "joint fit worsened observed contact geometry; retain the provisional map".into(),
        );
    }
    let residuals = problem
        .residual_vector(&problem.parameters)
        .ok_or("cannot evaluate fitted map")?;
    let jacobian = problem
        .jacobian_at(&problem.parameters)
        .ok_or("cannot evaluate fitted Jacobian")?;
    let svd = SVD::new(jacobian, true, true);
    let largest = svd.singular_values.get(0).copied().unwrap_or(0.0);
    let tolerance = largest * 1e-8;
    let rank = svd.rank(tolerance);
    let evidence_resolution = field_evidence_resolution(&svd, problem.parameters.len());
    let parameter_to_physical = problem.physical_jacobian(&problem.parameters);
    let covariance_unavailable_reason = if rank < problem.parameters.len() {
        Some(CovarianceUnavailableReason::RankDeficient)
    } else if independent_paths <= rank {
        Some(CovarianceUnavailableReason::InsufficientIndependentPaths)
    } else if parameter_to_physical.is_none() {
        Some(CovarianceUnavailableReason::ParameterTransformUnavailable)
    } else {
        None
    };
    let covariance = if let (None, Some(parameter_to_physical)) = (
        covariance_unavailable_reason,
        parameter_to_physical.as_ref(),
    ) {
        let inverse = svd
            .pseudo_inverse(tolerance)
            .map_err(|error| error.to_string())?;
        let variance = (residuals.norm_squared() / (independent_paths - rank) as f64).max(1e-8);
        let parameter_covariance = &inverse * inverse.transpose() * variance;
        // Centers depend on both their bounded coordinate and radius. Propagate
        // the full nonlinear decoding Jacobian, including those cross terms.
        let covariance =
            parameter_to_physical * parameter_covariance * parameter_to_physical.transpose();
        Some(covariance)
    } else {
        None
    };
    let mut squared_position = 0.0;
    let mut coordinates = 0;
    for shot in &problem.observations {
        if cancelled.load(Ordering::Relaxed) {
            return Err("planet fit cancelled".into());
        }
        let n = shot.points.len().saturating_sub(1);
        if n < 2 {
            continue;
        }
        let predicted = simulate_sampled_positions(&map, shot, world, n)?;
        for (observed, predicted) in shot.points[1..n].iter().zip(&predicted[1..]) {
            squared_position += (f64::from(observed.x) - predicted.x).powi(2)
                + (f64::from(observed.y) - predicted.y).powi(2);
            coordinates += 2;
        }
    }
    let contact_rmse = if contacts.is_empty() {
        0.0
    } else {
        (contacts
            .iter()
            .map(|contact| {
                let planet = &map.planets[contact.planet_index];
                ((planet.x - f64::from(contact.point.x))
                    .hypot(planet.y - f64::from(contact.point.y))
                    - planet.radius)
                    .powi(2)
            })
            .sum::<f64>()
            / contacts.len() as f64)
            .sqrt()
    };
    Ok(PlanetFit {
        map,
        covariance,
        covariance_unavailable_reason,
        position_rmse: if coordinates == 0 {
            0.0
        } else {
            (squared_position / coordinates as f64).sqrt()
        },
        contact_rmse,
        rank,
        independent_paths,
        evidence_resolution,
        parameter_to_physical,
        converged,
    })
}

fn field_evidence_resolution(
    svd: &SVD<f64, Dyn, Dyn>,
    parameter_count: usize,
) -> Option<DMatrix<f64>> {
    let v_t = svd.v_t.as_ref()?;
    if v_t.ncols() != parameter_count {
        return None;
    }
    let largest = svd.singular_values.get(0).copied().unwrap_or(0.0);
    let cutoff = largest * 1e-8;
    let components = svd
        .singular_values
        .len()
        .min(v_t.nrows())
        .min(parameter_count);
    let values = DVector::from_fn(components, |index, _| {
        let singular = svd.singular_values.get(index).copied().unwrap_or(0.0);
        if singular > cutoff {
            let information = singular * singular;
            information / (1.0 + information)
        } else {
            0.0
        }
    });
    let right_vectors = v_t.rows(0, components).transpose();
    Some(&right_vectors * DMatrix::from_diagonal(&values) * right_vectors.transpose())
}

fn encode_planets(map: &PlanetMap, world: WorldParameters) -> DVector<f64> {
    DVector::from_iterator(
        map.planets.len() * 3,
        map.planets.iter().flat_map(|planet| {
            let fraction = ((planet.radius - 20.0) / 20.0).clamp(1e-6, 1.0 - 1e-6);
            let radius = 20.0 + 20.0 * fraction;
            let x = ((planet.x - radius) / (world.battlefield_width - 2.0 * radius))
                .clamp(1e-6, 1.0 - 1e-6);
            let y = ((planet.y - radius) / (world.battlefield_height - 2.0 * radius))
                .clamp(1e-6, 1.0 - 1e-6);
            [
                (x / (1.0 - x)).ln(),
                (y / (1.0 - y)).ln(),
                (fraction / (1.0 - fraction)).ln(),
            ]
        }),
    )
}

// A displaced seed can put a path through a singularity. If it conflicts with
// known contacts, compare physically valid circles through those contacts using
// the same trajectory and surface objective as the subsequent joint fit.
fn contact_seeded_map(
    initial: &PlanetMap,
    observations: &[ShotObservation],
    contacts: &[ContactConstraint],
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> PlanetMap {
    let mut map = initial.clone();
    let score = |candidate: &PlanetMap| {
        let problem = Problem {
            parameters: encode_planets(candidate, world),
            observations: observations.to_vec(),
            contacts: contacts.to_vec(),
            world,
            cancelled,
            workers: 1,
        };
        problem
            .residual_vector(&problem.parameters)
            .map(|r| r.norm_squared())
            .unwrap_or(f64::INFINITY)
    };
    for index in 0..map.planets.len() {
        let associated: Vec<_> = contacts
            .iter()
            .filter(|c| c.planet_index == index)
            .collect();
        if associated.len() < 2 {
            continue;
        }
        let old = map.planets[index].clone();
        let contact_error: f64 = associated
            .iter()
            .map(|c| {
                ((old.x - f64::from(c.point.x)).hypot(old.y - f64::from(c.point.y)) - old.radius)
                    .powi(2)
            })
            .sum();
        if contact_error / associated.len() as f64 <= 4.0 {
            continue;
        }
        let mut best = old.clone();
        let mut best_score = score(&map);
        for (i, left) in associated.iter().enumerate() {
            for right in &associated[i + 1..] {
                let (x1, y1) = (f64::from(left.point.x), f64::from(left.point.y));
                let (x2, y2) = (f64::from(right.point.x), f64::from(right.point.y));
                let (dx, dy) = (x2 - x1, y2 - y1);
                let distance = dx.hypot(dy);
                if distance == 0.0 {
                    continue;
                }
                for radius_step in 20..=40 {
                    if cancelled.load(Ordering::Relaxed) {
                        return map;
                    }
                    let radius = f64::from(radius_step);
                    if distance > 2.0 * radius {
                        continue;
                    }
                    let height = (radius * radius - distance * distance / 4.0).sqrt();
                    for sign in [-1.0, 1.0] {
                        let candidate = Planet {
                            x: (x1 + x2) / 2.0 - sign * dy / distance * height,
                            y: (y1 + y2) / 2.0 + sign * dx / distance * height,
                            radius,
                            mass: radius.powi(3) / 10.0,
                        };
                        if candidate.x < radius
                            || candidate.x > world.battlefield_width - radius
                            || candidate.y < radius
                            || candidate.y > world.battlefield_height - radius
                        {
                            continue;
                        }
                        map.planets[index] = candidate.clone();
                        if maximum_overlap(&map) > 0.0 {
                            continue;
                        }
                        let value = score(&map);
                        if value < best_score {
                            best_score = value;
                            best = candidate;
                        }
                    }
                }
            }
        }
        map.planets[index] = best;
    }
    map
}

fn maximum_overlap(map: &PlanetMap) -> f64 {
    map.planets
        .iter()
        .enumerate()
        .flat_map(|(index, left)| {
            map.planets[index + 1..].iter().map(move |right| {
                left.radius + right.radius - (left.x - right.x).hypot(left.y - right.y)
            })
        })
        .fold(0.0, f64::max)
}

fn contact_violation(map: &PlanetMap, contacts: &[ContactConstraint]) -> f64 {
    contacts
        .iter()
        .map(|contact| {
            let planet = &map.planets[contact.planet_index];
            let signed = (planet.x - f64::from(contact.point.x))
                .hypot(planet.y - f64::from(contact.point.y))
                - planet.radius;
            signed.max(0.0).max(-signed - 0.5)
        })
        .fold(0.0, f64::max)
}

struct Problem<'a> {
    parameters: DVector<f64>,
    observations: Vec<ShotObservation>,
    contacts: Vec<ContactConstraint>,
    world: WorldParameters,
    cancelled: &'a AtomicBool,
    workers: usize,
}
impl Problem<'_> {
    fn decode(&self, parameters: &DVector<f64>) -> Option<PlanetMap> {
        let mut planets = Vec::new();
        for p in parameters.as_slice().chunks_exact(3) {
            let radius = 20.0 + 20.0 / (1.0 + (-p[2]).exp());
            let x = radius + (self.world.battlefield_width - 2.0 * radius) / (1.0 + (-p[0]).exp());
            let y = radius + (self.world.battlefield_height - 2.0 * radius) / (1.0 + (-p[1]).exp());
            if ![x, y, radius].iter().all(|v| v.is_finite()) {
                return None;
            }
            planets.push(Planet {
                x,
                y,
                radius,
                mass: radius.powi(3) / 10.0,
            });
        }
        Some(PlanetMap { planets })
    }
    fn physical_jacobian(&self, parameters: &DVector<f64>) -> Option<DMatrix<f64>> {
        let mut transform = DMatrix::zeros(parameters.len(), parameters.len());
        for column in 0..parameters.len() {
            let step = 1e-5 * (1.0 + parameters[column].abs());
            let mut plus = parameters.clone();
            let mut minus = parameters.clone();
            plus[column] += step;
            minus[column] -= step;
            let plus = self.decode(&plus)?;
            let minus = self.decode(&minus)?;
            for (index, (plus, minus)) in plus.planets.iter().zip(&minus.planets).enumerate() {
                transform[(index * 3, column)] = (plus.x - minus.x) / (2.0 * step);
                transform[(index * 3 + 1, column)] = (plus.y - minus.y) / (2.0 * step);
                transform[(index * 3 + 2, column)] = (plus.mass - minus.mass) / (2.0 * step);
            }
        }
        Some(transform)
    }
    fn residual_vector(&self, parameters: &DVector<f64>) -> Option<DVector<f64>> {
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let map = self.decode(parameters)?;
        let mut residuals = Vec::new();
        for shot in &self.observations {
            if self.cancelled.load(Ordering::Relaxed) {
                return None;
            }
            let n = shot.points.len().saturating_sub(1);
            if n < 2 {
                continue;
            }
            let predicted = simulate_sampled_positions(&map, shot, self.world, n).ok()?;
            let scale = (2 * (n - 1)) as f64;
            for (observed, predicted) in shot.points[1..n].iter().zip(&predicted[1..]) {
                for residual in [
                    f64::from(observed.x) - predicted.x,
                    f64::from(observed.y) - predicted.y,
                ] {
                    // Stable pseudo-Huber residual, equal to r close to zero.
                    let robust =
                        residual * (2.0 / ((1.0 + (residual / 5.0).powi(2)).sqrt() + 1.0)).sqrt();
                    residuals.push(robust / scale.sqrt());
                }
                for planet in &map.planets {
                    let distance =
                        (f64::from(observed.x) - planet.x).hypot(f64::from(observed.y) - planet.y);
                    residuals
                        .push((planet.radius - distance).max(0.0) * 2.0 / ((n - 1) as f64).sqrt());
                }
            }
        }
        for contact in &self.contacts {
            let planet = &map.planets[contact.planet_index];
            let signed = (f64::from(contact.point.x) - planet.x)
                .hypot(f64::from(contact.point.y) - planet.y)
                - planet.radius;
            residuals.push(if signed > 0.0 {
                signed * 2.0
            } else {
                (signed + 0.5).min(0.0) * 2.0
            });
        }
        for (index, left) in map.planets.iter().enumerate() {
            for right in &map.planets[index + 1..] {
                let overlap =
                    left.radius + right.radius - (left.x - right.x).hypot(left.y - right.y);
                residuals.push(overlap.max(0.0) * 2.0);
            }
        }
        residuals
            .iter()
            .all(|r| r.is_finite())
            .then(|| DVector::from_vec(residuals))
    }
    fn jacobian_at(&self, parameters: &DVector<f64>) -> Option<DMatrix<f64>> {
        let baseline = self.residual_vector(parameters)?;
        let columns = parameters.len();
        let workers = self.workers.max(1).min(columns.max(1));
        let chunk = columns.div_ceil(workers);
        let calculate = |start: usize, end: usize| -> Option<Vec<(usize, Vec<f64>)>> {
            let mut result = Vec::new();
            for column in start..end {
                let step = 1e-5 * (1.0 + parameters[column].abs());
                let mut plus = parameters.clone();
                let mut minus = parameters.clone();
                plus[column] += step;
                minus[column] -= step;
                let plus = self.residual_vector(&plus);
                let minus = self.residual_vector(&minus);
                if self.cancelled.load(Ordering::Relaxed) {
                    return None;
                }
                let mut values = Vec::with_capacity(baseline.len());
                for row in 0..baseline.len() {
                    values.push(match (&plus, &minus) {
                        (Some(plus), Some(minus)) => (plus[row] - minus[row]) / (2.0 * step),
                        (Some(plus), None) => (plus[row] - baseline[row]) / step,
                        (None, Some(minus)) => (baseline[row] - minus[row]) / step,
                        (None, None) => return None,
                    });
                }
                result.push((column, values));
            }
            Some(result)
        };
        let results = if workers == 1 {
            calculate(0, columns)?
        } else {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for start in (0..columns).step_by(chunk) {
                    let calculate = &calculate;
                    handles
                        .push(scope.spawn(move || calculate(start, (start + chunk).min(columns))));
                }
                let mut results = Vec::new();
                for handle in handles {
                    results.extend(handle.join().expect("Jacobian worker panicked")?);
                }
                Some(results)
            })?
        };
        if self.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        let mut jacobian = DMatrix::zeros(baseline.len(), columns);
        for (column, values) in results {
            for (row, value) in values.into_iter().enumerate() {
                jacobian[(row, column)] = value;
            }
        }
        Some(jacobian)
    }
}
impl LeastSquaresProblem<f64, Dyn, Dyn> for Problem<'_> {
    type ParameterStorage = VecStorage<f64, Dyn, U1>;
    type ResidualStorage = VecStorage<f64, Dyn, U1>;
    type JacobianStorage = VecStorage<f64, Dyn, Dyn>;
    fn set_params(&mut self, parameters: &DVector<f64>) {
        self.parameters.copy_from(parameters);
    }
    fn params(&self) -> DVector<f64> {
        self.parameters.clone()
    }
    fn residuals(&self) -> Option<DVector<f64>> {
        self.residual_vector(&self.parameters)
    }
    fn jacobian(&self) -> Option<DMatrix<f64>> {
        self.jacobian_at(&self.parameters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Planet;
    use crate::simulation::simulate_sampled_path;

    #[test]
    fn parallel_jacobian_matches_serial_and_respects_cancellation() {
        let world = WorldParameters::default();
        let planet = Planet {
            x: 500.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        };
        let map = PlanetMap {
            planets: vec![planet],
        };
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 4.0,
            points: vec![Point { x: 300.0, y: 450.0 }],
        };
        shot.points = simulate_sampled_path(&map, &shot, world, 12).unwrap();
        let cancelled = AtomicBool::new(false);
        let mut problem = Problem {
            parameters: encode_planets(&map, world),
            observations: vec![shot],
            contacts: vec![ContactConstraint {
                planet_index: 0,
                point: Point { x: 470.0, y: 500.0 },
            }],
            world,
            cancelled: &cancelled,
            workers: 1,
        };
        let serial = problem.jacobian_at(&problem.parameters).unwrap();
        problem.workers = 3;
        let parallel = problem.jacobian_at(&problem.parameters).unwrap();
        assert_eq!(serial, parallel);
        cancelled.store(true, Ordering::Relaxed);
        assert!(problem.jacobian_at(&problem.parameters).is_none());
    }

    #[test]
    fn jointly_refines_two_planets_and_ties_mass_to_radius() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.0,
                    y: 400.0,
                    radius: 27.0,
                    mass: 27f64.powi(3) / 10.0,
                },
                Planet {
                    x: 950.0,
                    y: 650.0,
                    radius: 34.0,
                    mass: 34f64.powi(3) / 10.0,
                },
            ],
        };
        let mut paths = Vec::new();
        for (x, y, angle) in [
            (300., 350., -15.),
            (600., 250., -80.),
            (800., 600., -10.),
            (1050., 500., -110.),
            (350., 500., 20.),
            (650., 450., 140.),
            (850., 800., 60.),
            (1100., 700., 160.),
        ] {
            let mut shot = ShotObservation {
                player_id: 1,
                angle_degrees: angle,
                velocity: 4.,
                points: vec![Point { x, y }],
            };
            shot.points = simulate_sampled_path(&truth, &shot, world, 18).unwrap();
            paths.push(shot);
        }
        let contacts = vec![
            ContactConstraint {
                planet_index: 0,
                point: Point { x: 473., y: 400. },
            },
            ContactConstraint {
                planet_index: 1,
                point: Point { x: 950., y: 616. },
            },
        ];
        let mut initial = truth.clone();
        initial.planets[0].x += 3.;
        initial.planets[0].radius = 29.;
        initial.planets[1].y -= 3.;
        initial.planets[1].radius = 32.;
        let few_paths = fit_planet_map(
            &initial,
            &paths[..4],
            &contacts,
            world,
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(
            few_paths.covariance.is_none(),
            "many correlated point rows cannot replace independent paths"
        );
        assert_eq!(
            few_paths.covariance_unavailable_reason,
            Some(CovarianceUnavailableReason::InsufficientIndependentPaths)
        );
        let fit =
            fit_planet_map(&initial, &paths, &contacts, world, &AtomicBool::new(false)).unwrap();
        for (got, expected) in fit.map.planets.iter().zip(&truth.planets) {
            assert!(
                (got.x - expected.x).hypot(got.y - expected.y) < 0.5,
                "{got:?}"
            );
            assert!((got.radius - expected.radius).abs() < 0.5, "{got:?}");
            assert!((got.mass - got.radius.powi(3) / 10.).abs() < 1e-9);
        }
        assert!(fit.position_rmse < 0.01, "{}", fit.position_rmse);
        assert!(fit.contact_rmse < 0.5);
        assert_eq!(fit.rank, 6);
        assert!(fit.covariance.is_some());
        assert_eq!(fit.covariance_unavailable_reason, None);
        assert!(fit.independent_paths > fit.rank);
        assert!(fit.evidence_resolution.as_ref().unwrap().trace() > 0.0);
        assert!(fit.parameter_to_physical.is_some());
        let repeated = paths
            .iter()
            .chain(paths.iter())
            .cloned()
            .collect::<Vec<_>>();
        let repeated_fit = fit_planet_map(
            &initial,
            &repeated,
            &contacts,
            world,
            &AtomicBool::new(false),
        )
        .unwrap();
        let covariance = fit.covariance.as_ref().unwrap();
        let repeated_covariance = repeated_fit.covariance.as_ref().unwrap();
        assert!(
            (covariance - repeated_covariance).norm() <= covariance.norm() * 1e-6,
            "replayed deterministic paths must not manufacture confidence"
        );
    }

    #[test]
    fn overlapping_seed_planets_are_rejected() {
        let initial = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.,
                    y: 500.,
                    radius: 30.,
                    mass: 2700.,
                },
                Planet {
                    x: 550.,
                    y: 500.,
                    radius: 30.,
                    mass: 2700.,
                },
            ],
        };
        let error = fit_planet_map(
            &initial,
            &[],
            &[ContactConstraint {
                planet_index: 0,
                point: Point { x: 470., y: 500. },
            }],
            WorldParameters::default(),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.contains("overlap"), "{error}");
    }

    #[test]
    fn arbitrary_finite_optimizer_steps_decode_inside_the_battlefield() {
        let cancelled = AtomicBool::new(false);
        let problem = Problem {
            parameters: DVector::from_vec(vec![100.0, -100.0, 0.0]),
            observations: vec![],
            contacts: vec![],
            world: WorldParameters::default(),
            cancelled: &cancelled,
            workers: 1,
        };
        let map = problem
            .decode(&problem.parameters)
            .expect("bounded coordinates must remain physical after optimizer steps");
        let planet = &map.planets[0];
        assert!(
            planet.x >= planet.radius
                && planet.x <= problem.world.battlefield_width - planet.radius
        );
        assert!(
            planet.y >= planet.radius
                && planet.y <= problem.world.battlefield_height - planet.radius
        );
    }

    #[test]
    fn biased_near_boundary_seed_refines_without_leaving_the_battlefield() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 35.0,
                y: 100.0,
                radius: 25.0,
                mass: 1562.5,
            }],
        };
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 70.0,
                y: 100.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let mut shots = Vec::new();
        for (x, y, angle) in [
            (100., 60., -150.),
            (120., 100., 180.),
            (90., 170., 120.),
            (150., 150., 160.),
        ] {
            let mut shot = ShotObservation {
                player_id: 1,
                angle_degrees: angle,
                velocity: 4.,
                points: vec![Point { x, y }],
            };
            shot.points = crate::simulation::simulate_shot(&truth, &shot, world, 12)
                .unwrap()
                .points;
            shots.push(shot);
        }
        let contacts = vec![
            ContactConstraint {
                planet_index: 0,
                point: Point { x: 10., y: 100. },
            },
            ContactConstraint {
                planet_index: 0,
                point: Point { x: 35., y: 75. },
            },
        ];
        let fit =
            fit_planet_map(&initial, &shots, &contacts, world, &AtomicBool::new(false)).unwrap();
        let planet = &fit.map.planets[0];
        assert!((planet.x - 35.).hypot(planet.y - 100.) < 2.0, "{planet:?}");
        assert!((planet.radius - 25.).abs() < 2.0, "{planet:?}");
        assert!(planet.x >= planet.radius);
        assert!(fit.position_rmse < 0.5, "{}", fit.position_rmse);
    }

    #[test]
    fn source_prior_boundary_and_battlefield_edge_remain_physical() {
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 20.,
                y: 100.,
                radius: 20.,
                mass: 800.,
            }],
        };
        let fit = fit_planet_map(
            &initial,
            &[],
            &[ContactConstraint {
                planet_index: 0,
                point: Point { x: 40., y: 100. },
            }],
            WorldParameters::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let planet = &fit.map.planets[0];
        assert!((20.0..=40.0).contains(&planet.radius));
        assert!(planet.x >= planet.radius);
    }

    #[test]
    fn pre_cancelled_fit_stops_instead_of_publishing_a_map() {
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 500.,
                y: 500.,
                radius: 30.,
                mass: 2700.,
            }],
        };
        let result = fit_planet_map(
            &initial,
            &[],
            &[ContactConstraint {
                planet_index: 0,
                point: Point { x: 470., y: 500. },
            }],
            WorldParameters::default(),
            &AtomicBool::new(true),
        );
        assert_eq!(result.unwrap_err(), "planet fit cancelled");
    }

    #[test]
    fn one_surface_contact_has_unidentified_directions() {
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 500.,
                y: 500.,
                radius: 30.,
                mass: 2700.,
            }],
        };
        let fit = fit_planet_map(
            &initial,
            &[],
            &[ContactConstraint {
                planet_index: 0,
                point: Point { x: 470., y: 500. },
            }],
            WorldParameters::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(fit.covariance.is_none());
        assert!(fit.rank < 3);
        assert_eq!(
            fit.covariance_unavailable_reason,
            Some(CovarianceUnavailableReason::RankDeficient)
        );
    }
}
