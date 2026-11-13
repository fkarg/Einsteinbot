use crate::map::PlanetMap;
use crate::protocol::Planet;
use crate::simulation::{
    ShotObservation, WorldParameters, reconstruct_acceleration, simulate_sampled_positions,
};
use levenberg_marquardt::{LeastSquaresProblem, LevenbergMarquardt};
use nalgebra::{DMatrix, DVector, Dyn, U1, VecStorage, linalg::SVD};
use std::error::Error;
use std::fmt;

const PARAMETERS_PER_PLANET: usize = 3;
/// Position observations are f32; exact synthetic fits still need a nonzero noise floor.
const POSITION_NOISE_FLOOR: f64 = 1e-4;

#[derive(Clone, Debug, PartialEq)]
pub struct FitResult {
    /// Best-fit physical source configuration. Radius is held at its prior value in this fit.
    pub map: PlanetMap,
    /// Parameter order is `[center_x, center_y, mass]` for each planet in map order.
    /// Local covariance uses a `1e-4` world-unit observation-noise floor. `None` means the fit
    /// Jacobian is rank deficient or has no residual degrees of freedom.
    pub covariance: Option<DMatrix<f64>>,
    pub rank: usize,
    pub singular_values: Vec<f64>,
    pub residual_rmse: f64,
    pub converged: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AccelerationResidual {
    pub position: crate::map::Vec2,
    pub observed: crate::map::Vec2,
    pub predicted: crate::map::Vec2,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FitError(String);

impl fmt::Display for FitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for FitError {}

/// Compare acceleration reconstructed from a shot's regular samples with the planet field.
pub fn acceleration_residuals(
    map: &PlanetMap,
    observation: &ShotObservation,
) -> Result<Vec<AccelerationResidual>, FitError> {
    reconstruct_acceleration(&observation.points)
        .into_iter()
        .map(|(position, observed)| {
            let predicted = map
                .acceleration(position)
                .map_err(|error| FitError(error.to_string()))?;
            Ok(AccelerationResidual {
                position,
                observed,
                predicted,
            })
        })
        .collect()
}

/// Batch-fit centers and gravity weights for a fixed candidate source count.
///
/// The initial map supplies the source count and starting positions. Radius stays at its prior
/// value because smooth path curvature does not constrain collision boundaries. Each shot's
/// final point is omitted from the fit because its substep timestamp and termination cause are
/// absent from the protocol. Source-count comparison and radius fitting are separate stages.
pub fn fit_fixed_count(
    initial_map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
) -> Result<FitResult, FitError> {
    if initial_map.planets.is_empty() {
        return Err(FitError(
            "candidate map must contain at least one planet".to_owned(),
        ));
    }
    if observations.is_empty() {
        return Err(FitError(
            "at least one shot observation is required".to_owned(),
        ));
    }
    if world.battlefield_width <= 0.0
        || world.battlefield_height <= 0.0
        || world.substeps_per_tick == 0
    {
        return Err(FitError(
            "world dimensions and substep count must be positive".to_owned(),
        ));
    }
    for planet in &initial_map.planets {
        if !planet.x.is_finite()
            || !planet.y.is_finite()
            || !planet.radius.is_finite()
            || planet.radius <= 0.0
            || !planet.mass.is_finite()
            || planet.mass <= 0.0
            || planet.radius * 2.0 >= world.battlefield_width
            || planet.radius * 2.0 >= world.battlefield_height
            || planet.x < planet.radius
            || planet.x > world.battlefield_width - planet.radius
            || planet.y < planet.radius
            || planet.y > world.battlefield_height - planet.radius
        {
            return Err(FitError(
                "initial planet parameters must be finite and positive".to_owned(),
            ));
        }
    }

    let parameters = encode_map(initial_map, world);
    let parameter_count = parameters.len();
    let residual_count = usable_residual_count(observations);
    if residual_count < parameter_count {
        return Err(FitError(format!(
            "need at least {parameter_count} regular position residuals to fit this candidate map; got {residual_count}"
        )));
    }
    if residual_count == 0 {
        return Err(FitError(
            "shot observations contain no full-tick positions".to_owned(),
        ));
    }

    let problem = TrajectoryFitProblem {
        initial_map: initial_map.clone(),
        observations: observations.to_vec(),
        world,
        parameters,
    };
    let (problem, report) = LevenbergMarquardt::new().minimize(problem);
    let fitted_map = decode_map(&problem.initial_map, &problem.parameters, world)?;
    let residuals = problem
        .residual_vector(&problem.parameters)
        .ok_or_else(|| FitError("could not evaluate fitted trajectory residuals".to_owned()))?;
    let jacobian = problem
        .finite_difference_jacobian(&problem.parameters)
        .ok_or_else(|| FitError("could not evaluate fit Jacobian".to_owned()))?;
    let residual_rmse = (residuals.norm_squared() / residuals.len() as f64).sqrt();
    let svd = SVD::new(jacobian, true, true);
    let largest_singular_value = svd.singular_values.get(0).copied().unwrap_or(0.0);
    let rank_tolerance = largest_singular_value * 1e-8;
    let rank = svd.rank(rank_tolerance);
    let singular_values = svd.singular_values.iter().copied().collect::<Vec<_>>();
    let covariance = if rank == parameter_count && residuals.len() > parameter_count {
        let residual_variance = (residuals.norm_squared()
            / (residuals.len() - parameter_count) as f64)
            .max(POSITION_NOISE_FLOOR.powi(2));
        let pseudo_inverse = svd
            .pseudo_inverse(rank_tolerance)
            .map_err(|error| FitError(error.to_owned()))?;
        let covariance_in_optimizer_space =
            (&pseudo_inverse * pseudo_inverse.transpose()) * residual_variance;
        Some(transform_covariance(
            covariance_in_optimizer_space,
            &problem.parameters,
            &problem.initial_map,
            world,
        ))
    } else {
        None
    };

    Ok(FitResult {
        map: fitted_map,
        covariance,
        rank,
        singular_values,
        residual_rmse,
        converged: report.termination.was_successful(),
    })
}

struct TrajectoryFitProblem {
    initial_map: PlanetMap,
    observations: Vec<ShotObservation>,
    world: WorldParameters,
    parameters: DVector<f64>,
}

impl TrajectoryFitProblem {
    fn residual_vector(&self, parameters: &DVector<f64>) -> Option<DVector<f64>> {
        let map = decode_map(&self.initial_map, parameters, self.world).ok()?;
        let mut residuals = Vec::with_capacity(usable_residual_count(&self.observations));
        for shot in &self.observations {
            let usable_samples = shot.points.len().saturating_sub(1);
            if usable_samples < 2 {
                continue;
            }
            let predicted =
                simulate_sampled_positions(&map, shot, self.world, usable_samples).ok()?;
            for (observed, predicted) in shot.points[1..usable_samples].iter().zip(&predicted[1..])
            {
                residuals.push(f64::from(observed.x) - predicted.x);
                residuals.push(f64::from(observed.y) - predicted.y);
            }
        }
        Some(DVector::from_vec(residuals))
    }

    fn finite_difference_jacobian(&self, parameters: &DVector<f64>) -> Option<DMatrix<f64>> {
        let baseline = self.residual_vector(parameters)?;
        let mut jacobian = DMatrix::zeros(baseline.len(), parameters.len());
        for parameter_index in 0..parameters.len() {
            let step = 1e-5 * (1.0 + parameters[parameter_index].abs());
            let mut plus = parameters.clone();
            let mut minus = parameters.clone();
            plus[parameter_index] += step;
            minus[parameter_index] -= step;
            let residual_plus = self.residual_vector(&plus)?;
            let residual_minus = self.residual_vector(&minus)?;
            for row in 0..baseline.len() {
                jacobian[(row, parameter_index)] =
                    (residual_plus[row] - residual_minus[row]) / (2.0 * step);
            }
        }
        Some(jacobian)
    }
}

impl LeastSquaresProblem<f64, Dyn, Dyn> for TrajectoryFitProblem {
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
        self.finite_difference_jacobian(&self.parameters)
    }
}

fn usable_residual_count(observations: &[ShotObservation]) -> usize {
    observations
        .iter()
        .map(|shot| shot.points.len().saturating_sub(2) * 2)
        .sum()
}

fn encode_map(map: &PlanetMap, world: WorldParameters) -> DVector<f64> {
    let mut parameters = Vec::with_capacity(map.planets.len() * PARAMETERS_PER_PLANET);
    for planet in &map.planets {
        let x_span = world.battlefield_width - 2.0 * planet.radius;
        let y_span = world.battlefield_height - 2.0 * planet.radius;
        parameters.push(logit(
            ((planet.x - planet.radius) / x_span).clamp(1e-8, 1.0 - 1e-8),
        ));
        parameters.push(logit(
            ((planet.y - planet.radius) / y_span).clamp(1e-8, 1.0 - 1e-8),
        ));
        parameters.push(planet.mass.ln());
    }
    DVector::from_vec(parameters)
}

fn decode_map(
    prior: &PlanetMap,
    parameters: &DVector<f64>,
    world: WorldParameters,
) -> Result<PlanetMap, FitError> {
    if parameters.len() != prior.planets.len() * PARAMETERS_PER_PLANET {
        return Err(FitError(
            "parameter vector does not match candidate source count".to_owned(),
        ));
    }
    let planets = prior
        .planets
        .iter()
        .enumerate()
        .map(|(index, prior_planet)| {
            let offset = index * PARAMETERS_PER_PLANET;
            let x = prior_planet.radius
                + (world.battlefield_width - 2.0 * prior_planet.radius)
                    * sigmoid(parameters[offset]);
            let y = prior_planet.radius
                + (world.battlefield_height - 2.0 * prior_planet.radius)
                    * sigmoid(parameters[offset + 1]);
            let mass = parameters[offset + 2].exp();
            if !x.is_finite() || !y.is_finite() || !mass.is_finite() || mass <= 0.0 {
                return Err(FitError(
                    "optimizer produced non-finite planet parameters".to_owned(),
                ));
            }
            Ok(Planet {
                x,
                y,
                radius: prior_planet.radius,
                mass,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PlanetMap { planets })
}

fn transform_covariance(
    mut covariance: DMatrix<f64>,
    parameters: &DVector<f64>,
    prior: &PlanetMap,
    world: WorldParameters,
) -> DMatrix<f64> {
    let scales = (0..parameters.len())
        .map(|index| match index % PARAMETERS_PER_PLANET {
            0 => {
                let value = sigmoid(parameters[index]);
                let radius = prior.planets[index / PARAMETERS_PER_PLANET].radius;
                (world.battlefield_width - 2.0 * radius) * value * (1.0 - value)
            }
            1 => {
                let value = sigmoid(parameters[index]);
                let radius = prior.planets[index / PARAMETERS_PER_PLANET].radius;
                (world.battlefield_height - 2.0 * radius) * value * (1.0 - value)
            }
            _ => parameters[index].exp(),
        })
        .collect::<Vec<_>>();
    for row in 0..covariance.nrows() {
        for column in 0..covariance.ncols() {
            covariance[(row, column)] *= scales[row] * scales[column];
        }
    }
    covariance
}

fn sigmoid(value: f64) -> f64 {
    if value >= 0.0 {
        1.0 / (1.0 + (-value).exp())
    } else {
        let exp = value.exp();
        exp / (1.0 + exp)
    }
}

fn logit(value: f64) -> f64 {
    (value / (1.0 - value)).ln()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simulation::simulate_sampled_path;

    fn observation(
        map: &PlanetMap,
        start: (f32, f32),
        angle: f64,
        velocity: f64,
    ) -> ShotObservation {
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: angle,
            velocity,
            points: vec![crate::protocol::Point {
                x: start.0,
                y: start.1,
            }],
        };
        shot.points = simulate_sampled_path(map, &shot, WorldParameters::default(), 40).unwrap();
        shot
    }

    #[test]
    fn fixed_count_fit_recovers_source_center_and_weight_from_varied_paths() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 450.0,
                radius: 8.0,
                mass: 40_000.0,
            }],
        };
        let observations = vec![
            observation(&truth, (200.0, 200.0), 25.0, 8.0),
            observation(&truth, (800.0, 250.0), 150.0, 8.0),
            observation(&truth, (300.0, 850.0), -40.0, 9.0),
        ];
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 510.0,
                y: 440.0,
                radius: 8.0,
                mass: 36_000.0,
            }],
        };

        let fit = fit_fixed_count(&initial, &observations, world).unwrap();

        assert!(fit.converged);
        assert!(
            fit.residual_rmse < 0.1,
            "fit rmse was {}",
            fit.residual_rmse
        );
        assert!((fit.map.planets[0].x - truth.planets[0].x).abs() < 1.0);
        assert!((fit.map.planets[0].y - truth.planets[0].y).abs() < 1.0);
        assert!((fit.map.planets[0].mass - truth.planets[0].mass).abs() < 100.0);
        assert_eq!(fit.rank, 3);
        let covariance = fit.covariance.expect("well-constrained fit has covariance");
        assert!(
            covariance.trace() > 0.0,
            "quantization floor prevents false certainty"
        );
    }

    #[test]
    fn fit_refuses_fewer_residuals_than_source_parameters() {
        let initial = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 450.0,
                radius: 8.0,
                mass: 40_000.0,
            }],
        };
        let observation = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![crate::protocol::Point { x: 1.0, y: 2.0 }; 3],
        };

        assert!(fit_fixed_count(&initial, &[observation], WorldParameters::default()).is_err());
    }

    #[test]
    fn acceleration_residuals_compare_path_curvature_with_planet_field() {
        let map = PlanetMap { planets: vec![] };
        let shot = observation(&map, (100.0, 100.0), 0.0, 10.0);

        let residuals = acceleration_residuals(&map, &shot).unwrap();

        assert_eq!(residuals.len(), shot.points.len() - 3);
        assert!(residuals.iter().all(|residual| {
            residual.observed.norm_squared() < 1e-9 && residual.predicted.norm_squared() == 0.0
        }));
    }
}
