use crate::event_store::StoredEvent;
use crate::map::PlanetMap;
use crate::protocol::{Planet, RecordedEvent, ServerEvent};
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

#[derive(Clone, Debug, PartialEq)]
pub struct SourceCountCandidate {
    pub result: FitResult,
    /// `None` means no holdout observations were supplied.
    pub validation_rmse: Option<f64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceCountStopReason {
    ReachedMaximum,
    NoPositiveResidual,
    InsufficientResiduals { required: usize, available: usize },
    CandidateFitFailed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceCountSearch {
    /// Candidate maps are proposals, not proof that each source exists.
    pub candidates: Vec<SourceCountCandidate>,
    pub stop_reason: SourceCountStopReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FitError(String);

impl fmt::Display for FitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for FitError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LiveEstimatorSettings {
    pub max_sources: usize,
    pub radius_prior: f64,
    pub grid_spacing: f64,
    /// Hold out every Nth unique trajectory once enough data are available; zero disables it.
    pub validation_every: usize,
    /// Refit only after this many new unique trajectories since the last fit; one means refit
    /// after every trajectory. Batching bounds refit cost as accumulated observations grow.
    pub refit_batch: usize,
}

impl Default for LiveEstimatorSettings {
    fn default() -> Self {
        Self {
            max_sources: 4,
            radius_prior: 30.0,
            grid_spacing: 80.0,
            validation_every: 5,
            refit_batch: 4,
        }
    }
}

/// What `LiveEstimator::observe` did with one completed-shot event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationOutcome {
    /// An identical trajectory was already accumulated.
    Duplicate,
    /// Accumulated without reaching the next refit batch.
    Accumulated,
    /// Accumulated and the candidate search was refit.
    Refitted,
}

/// Rebuild candidate planet maps from the unique completed paths observed in this map epoch.
#[derive(Clone, Debug)]
pub struct LiveEstimator {
    world: WorldParameters,
    settings: LiveEstimatorSettings,
    observations: Vec<ShotObservation>,
    pending_since_last_fit: usize,
    search: Option<SourceCountSearch>,
    last_fit_error: Option<String>,
}

impl Default for LiveEstimator {
    fn default() -> Self {
        Self::new(WorldParameters::default(), LiveEstimatorSettings::default())
    }
}

impl LiveEstimator {
    pub fn new(world: WorldParameters, settings: LiveEstimatorSettings) -> Self {
        Self {
            world,
            settings,
            observations: Vec::new(),
            pending_since_last_fit: 0,
            search: None,
            last_fit_error: None,
        }
    }

    /// Add one completed shot and refit the accumulated distinct paths once enough new
    /// trajectories have arrived. Exact replays are ignored so repeated identical shots do not
    /// falsely shrink covariance as if they were independent evidence.
    pub fn observe(&mut self, event: &ServerEvent) -> ObservationOutcome {
        let Ok(observation) = ShotObservation::try_from(event) else {
            return ObservationOutcome::Duplicate;
        };
        if self.observations.contains(&observation) {
            return ObservationOutcome::Duplicate;
        }
        self.observations.push(observation);
        self.pending_since_last_fit += 1;
        let should_refit = self.search.is_none()
            || self.pending_since_last_fit >= self.settings.refit_batch.max(1);
        if !should_refit {
            return ObservationOutcome::Accumulated;
        }
        self.pending_since_last_fit = 0;
        let (training, validation) =
            split_live_observations(&self.observations, self.settings.validation_every);
        match fit_greedy_source_counts(
            &training,
            &validation,
            self.world,
            self.settings.max_sources,
            self.settings.radius_prior,
            self.settings.grid_spacing,
        ) {
            Ok(search) => {
                self.search = Some(search);
                self.last_fit_error = None;
            }
            Err(error) => self.last_fit_error = Some(error.to_string()),
        }
        ObservationOutcome::Refitted
    }

    /// Select the candidate map to publish as the current belief: lowest held-out score first;
    /// without holdout shots, the highest fitted source count (training error is monotone in
    /// sources and cannot select).
    pub fn best_map(&self) -> Option<(PlanetMap, Option<DMatrix<f64>>)> {
        let search = self.search.as_ref()?;
        search
            .candidates
            .iter()
            .filter(|candidate| candidate.validation_rmse.is_some())
            .min_by(|left, right| {
                left.validation_rmse
                    .unwrap_or(f64::INFINITY)
                    .total_cmp(&right.validation_rmse.unwrap_or(f64::INFINITY))
            })
            .or_else(|| search.candidates.last())
            .map(|candidate| {
                (
                    candidate.result.map.clone(),
                    candidate.result.covariance.clone(),
                )
            })
    }

    pub fn observations(&self) -> &[ShotObservation] {
        &self.observations
    }

    pub fn search(&self) -> Option<&SourceCountSearch> {
        self.search.as_ref()
    }

    pub fn last_fit_error(&self) -> Option<&str> {
        self.last_fit_error.as_deref()
    }

    pub fn reset(&mut self) {
        self.observations.clear();
        self.pending_since_last_fit = 0;
        self.search = None;
        self.last_fit_error = None;
    }
}

fn split_live_observations(
    observations: &[ShotObservation],
    validation_every: usize,
) -> (Vec<ShotObservation>, Vec<ShotObservation>) {
    if validation_every < 2 || observations.len() < validation_every {
        return (observations.to_vec(), Vec::new());
    }
    let mut training = Vec::new();
    let mut validation = Vec::new();
    for (index, observation) in observations.iter().enumerate() {
        if (index + 1) % validation_every == 0 {
            validation.push(observation.clone());
        } else {
            training.push(observation.clone());
        }
    }
    if training.is_empty() || validation.is_empty() {
        (observations.to_vec(), Vec::new())
    } else {
        (training, validation)
    }
}

/// Extract completed protocol-version-8 trajectories from one ordered event capture.
pub fn shot_observations(events: &[StoredEvent]) -> Vec<ShotObservation> {
    events
        .iter()
        .filter_map(|stored| match &stored.event {
            RecordedEvent::Inbound(event @ ServerEvent::ShotFin { .. }) => {
                ShotObservation::try_from(event).ok()
            }
            _ => None,
        })
        .collect()
}

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

/// Find one deterministic point-source seed that best explains the current acceleration residual.
///
/// This coarse-grid initializer avoids sampling random planet layouts. Its output is a starting
/// candidate only; the full trajectory fit should refine the source and assess whether it helps.
pub fn seed_planet_from_residuals(
    current_map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
    radius_prior: f64,
    grid_spacing: f64,
) -> Result<Planet, FitError> {
    try_seed_planet_from_residuals(current_map, observations, world, radius_prior, grid_spacing)?
        .ok_or_else(|| FitError("no positive point-source residual fit found".to_owned()))
}

fn try_seed_planet_from_residuals(
    current_map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
    radius_prior: f64,
    grid_spacing: f64,
) -> Result<Option<Planet>, FitError> {
    if observations.is_empty() {
        return Err(FitError(
            "at least one shot observation is required".to_owned(),
        ));
    }
    if !radius_prior.is_finite()
        || radius_prior <= 0.0
        || grid_spacing <= 0.0
        || !grid_spacing.is_finite()
        || !world.battlefield_width.is_finite()
        || !world.battlefield_height.is_finite()
        || world.battlefield_width <= 2.0 * radius_prior
        || world.battlefield_height <= 2.0 * radius_prior
    {
        return Err(FitError(
            "seed radius, grid spacing, or world dimensions are invalid".to_owned(),
        ));
    }

    let samples = observations
        .iter()
        .flat_map(|observation| reconstruct_acceleration(&observation.points))
        .map(|(position, observed)| {
            let predicted = current_map
                .acceleration(position)
                .map_err(|error| FitError(error.to_string()))?;
            Ok((
                position,
                crate::map::Vec2::new(observed.x - predicted.x, observed.y - predicted.y),
            ))
        })
        .collect::<Result<Vec<_>, FitError>>()?;
    if samples.is_empty() {
        return Err(FitError(
            "shot observations contain no regular acceleration samples".to_owned(),
        ));
    }

    let mut best: Option<(f64, Planet)> = None;
    let mut center_x = radius_prior;
    while center_x <= world.battlefield_width - radius_prior {
        let mut center_y = radius_prior;
        while center_y <= world.battlefield_height - radius_prior {
            let mut numerator = 0.0;
            let mut denominator = 0.0;
            for (position, residual) in &samples {
                let dx = center_x - position.x;
                let dy = center_y - position.y;
                let distance_squared = dx * dx + dy * dy;
                if distance_squared == 0.0 {
                    continue;
                }
                let scale = 1.0 / (distance_squared * distance_squared.sqrt());
                let gx = dx * scale;
                let gy = dy * scale;
                numerator += residual.x * gx + residual.y * gy;
                denominator += gx * gx + gy * gy;
            }
            if denominator > 0.0 && numerator > 0.0 {
                let mass = numerator / denominator;
                let score = numerator * numerator / denominator;
                if mass.is_finite()
                    && score.is_finite()
                    && best
                        .as_ref()
                        .is_none_or(|(best_score, _)| score > *best_score)
                {
                    best = Some((
                        score,
                        Planet {
                            x: center_x,
                            y: center_y,
                            radius: radius_prior,
                            mass,
                        },
                    ));
                }
            }
            center_y += grid_spacing;
        }
        center_x += grid_spacing;
    }

    Ok(best.map(|(_, planet)| planet))
}

/// Greedily propose and fit increasing fixed source counts from acceleration residuals.
///
/// Returns candidate maps up to the stopping point, preserving alternatives for later comparison.
/// Any positive projected residual can produce a proposal; held-out scores should be used to
/// compare them. If validation observations are provided, each fit is scored on those shots.
pub fn fit_greedy_source_counts(
    training: &[ShotObservation],
    validation: &[ShotObservation],
    world: WorldParameters,
    max_sources: usize,
    radius_prior: f64,
    grid_spacing: f64,
) -> Result<SourceCountSearch, FitError> {
    if training.is_empty() {
        return Err(FitError(
            "at least one training shot is required".to_owned(),
        ));
    }
    if max_sources == 0 || max_sources > 64 {
        return Err(FitError("max_sources must be between 1 and 64".to_owned()));
    }

    let mut fitted_map = PlanetMap { planets: vec![] };
    let mut candidates = Vec::with_capacity(max_sources);
    let mut stop_reason = SourceCountStopReason::ReachedMaximum;
    for source_count in 1..=max_sources {
        let required = source_count * PARAMETERS_PER_PLANET;
        let available = usable_residual_count(training);
        if available < required {
            stop_reason = SourceCountStopReason::InsufficientResiduals {
                required,
                available,
            };
            break;
        }
        let Some(seed) = try_seed_planet_from_residuals(
            &fitted_map,
            training,
            world,
            radius_prior,
            grid_spacing,
        )?
        else {
            stop_reason = SourceCountStopReason::NoPositiveResidual;
            break;
        };
        fitted_map.planets.push(seed);
        let result = match fit_fixed_count(&fitted_map, training, world) {
            Ok(result) => result,
            Err(error) => {
                stop_reason = SourceCountStopReason::CandidateFitFailed(error.to_string());
                break;
            }
        };
        let validation_rmse = if validation.is_empty() {
            None
        } else {
            Some(position_rmse(&result.map, validation, world)?)
        };
        fitted_map = result.map.clone();
        candidates.push(SourceCountCandidate {
            result,
            validation_rmse,
        });
    }
    Ok(SourceCountSearch {
        candidates,
        stop_reason,
    })
}

pub fn position_rmse(
    map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
) -> Result<f64, FitError> {
    let mut squared_error = 0.0;
    let mut coordinate_count = 0;
    for shot in observations {
        let Some((shot_squared_error, shot_coordinate_count)) =
            shot_position_squared_error(map, shot, world)?
        else {
            continue;
        };
        squared_error += shot_squared_error;
        coordinate_count += shot_coordinate_count;
    }
    if coordinate_count == 0 {
        return Err(FitError(
            "validation shots contain no regular positions".to_owned(),
        ));
    }
    Ok((squared_error / coordinate_count as f64).sqrt())
}

/// Position RMSE for one completed trajectory, excluding its untimed terminal sample.
pub fn shot_position_rmse(
    map: &PlanetMap,
    observation: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<f64>, FitError> {
    shot_position_squared_error(map, observation, world).map(|result| {
        result.map(|(squared_error, coordinate_count)| {
            (squared_error / coordinate_count as f64).sqrt()
        })
    })
}

fn shot_position_squared_error(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<(f64, usize)>, FitError> {
    let usable_samples = shot.points.len().saturating_sub(1);
    if usable_samples < 2 {
        return Ok(None);
    }
    let predicted =
        simulate_sampled_positions(map, shot, world, usable_samples).map_err(FitError)?;
    let mut squared_error = 0.0;
    let mut coordinate_count = 0;
    for (observed, predicted) in shot.points[1..usable_samples].iter().zip(&predicted[1..]) {
        squared_error += (f64::from(observed.x) - predicted.x).powi(2);
        squared_error += (f64::from(observed.y) - predicted.y).powi(2);
        coordinate_count += 2;
    }
    Ok(Some((squared_error, coordinate_count)))
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
    fn live_estimator_refits_on_first_observation_and_in_batches() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 450.0,
                radius: 30.0,
                mass: 40_000.0,
            }],
        };
        let shots = [
            observation(&truth, (200.0, 200.0), 25.0, 8.0),
            observation(&truth, (800.0, 250.0), 150.0, 8.0),
            observation(&truth, (300.0, 850.0), -40.0, 9.0),
            observation(&truth, (900.0, 700.0), 80.0, 7.0),
            observation(&truth, (150.0, 600.0), -110.0, 8.0),
        ];
        let mut live = LiveEstimator::new(
            world,
            LiveEstimatorSettings {
                max_sources: 1,
                radius_prior: 30.0,
                grid_spacing: 80.0,
                validation_every: 0,
                refit_batch: 2,
            },
        );

        let event = |shot: &ShotObservation| ServerEvent::ShotFin {
            player_id: shot.player_id,
            angle: shot.angle_degrees,
            velocity: shot.velocity,
            points: shot.points.clone(),
        };

        assert_eq!(
            live.observe(&event(&shots[0])),
            ObservationOutcome::Refitted
        );
        assert!(live.best_map().is_some());
        assert_eq!(
            live.observe(&event(&shots[1])),
            ObservationOutcome::Accumulated
        );
        assert_eq!(
            live.observe(&event(&shots[2])),
            ObservationOutcome::Refitted
        );
        assert_eq!(
            live.observe(&event(&shots[3])),
            ObservationOutcome::Accumulated
        );
        assert_eq!(
            live.observe(&event(&shots[4])),
            ObservationOutcome::Refitted
        );
        let duplicate = ServerEvent::ShotFin {
            player_id: shots[4].player_id,
            angle: shots[4].angle_degrees,
            velocity: shots[4].velocity,
            points: shots[4].points.clone(),
        };
        assert_eq!(live.observe(&duplicate), ObservationOutcome::Duplicate);
        assert_eq!(live.observations().len(), 5);
        assert_eq!(live.search().unwrap().candidates.len(), 1);
    }

    #[test]
    fn live_estimator_publishes_lowest_validation_rmse_then_highest_source_count() {
        let fit = |planets: usize, residual_rmse: f64| FitResult {
            map: PlanetMap {
                planets: vec![
                    Planet {
                        x: 0.0,
                        y: 0.0,
                        radius: 30.0,
                        mass: 1_000.0,
                    };
                    planets
                ],
            },
            covariance: None,
            rank: 3 * planets,
            singular_values: vec![],
            residual_rmse,
            converged: true,
        };
        let search = SourceCountSearch {
            candidates: vec![
                SourceCountCandidate {
                    result: fit(1, 10.0),
                    validation_rmse: Some(9.0),
                },
                SourceCountCandidate {
                    result: fit(2, 5.0),
                    validation_rmse: Some(4.0),
                },
                SourceCountCandidate {
                    result: fit(3, 1.0),
                    validation_rmse: None,
                },
            ],
            stop_reason: SourceCountStopReason::ReachedMaximum,
        };
        let mut live =
            LiveEstimator::new(WorldParameters::default(), LiveEstimatorSettings::default());
        live.search = Some(search);

        let (selected_planets, covariance) = live.best_map().unwrap();
        assert_eq!(selected_planets.planets.len(), 2);
        assert!(covariance.is_none());

        let no_holdout = SourceCountSearch {
            candidates: vec![
                SourceCountCandidate {
                    result: fit(1, 8.0),
                    validation_rmse: None,
                },
                SourceCountCandidate {
                    result: fit(2, 2.0),
                    validation_rmse: None,
                },
            ],
            stop_reason: SourceCountStopReason::ReachedMaximum,
        };
        live.search = Some(no_holdout);
        let selected = live.best_map().unwrap();
        assert_eq!(selected.0.planets.len(), 2);
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
    fn extracts_only_completed_shots_with_launch_parameters_from_capture() {
        let events = vec![
            StoredEvent {
                sequence: 0,
                timestamp_unix_ms: 10,
                bot_name: None,
                event: RecordedEvent::Inbound(ServerEvent::ShotBegin {
                    player_id: 2,
                    angle: 30.0,
                    velocity: 8.0,
                }),
            },
            StoredEvent {
                sequence: 1,
                timestamp_unix_ms: 11,
                bot_name: None,
                event: RecordedEvent::Inbound(ServerEvent::ShotFin {
                    player_id: 2,
                    angle: 30.0,
                    velocity: 8.0,
                    points: vec![
                        crate::protocol::Point { x: 1.0, y: 2.0 },
                        crate::protocol::Point { x: 3.0, y: 4.0 },
                    ],
                }),
            },
        ];

        let shots = shot_observations(&events);

        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].player_id, 2);
        assert_eq!(shots[0].angle_degrees, 30.0);
        assert_eq!(shots[0].velocity, 8.0);
        assert_eq!(shots[0].points[0].x, 1.0);
    }

    #[test]
    fn deterministic_residual_seed_provides_a_starting_planet_for_batch_fit() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 450.0,
                radius: 30.0,
                mass: 40_000.0,
            }],
        };
        let observations = vec![
            observation(&truth, (200.0, 200.0), 25.0, 8.0),
            observation(&truth, (800.0, 250.0), 150.0, 8.0),
            observation(&truth, (300.0, 850.0), -40.0, 9.0),
        ];

        let seed = seed_planet_from_residuals(
            &PlanetMap { planets: vec![] },
            &observations,
            world,
            30.0,
            80.0,
        )
        .unwrap();
        let seeded_map = PlanetMap {
            planets: vec![seed],
        };
        let fit = fit_fixed_count(&seeded_map, &observations, world).unwrap();

        assert!(
            fit.residual_rmse < 0.1,
            "fit rmse was {}",
            fit.residual_rmse
        );
        assert!((fit.map.planets[0].x - truth.planets[0].x).abs() < 1.0);
        assert!((fit.map.planets[0].y - truth.planets[0].y).abs() < 1.0);
        assert!((fit.map.planets[0].mass - truth.planets[0].mass).abs() < 100.0);
    }

    #[test]
    fn acceleration_residual_seed_can_propose_a_second_source() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.0,
                    y: 450.0,
                    radius: 30.0,
                    mass: 40_000.0,
                },
                Planet {
                    x: 1_300.0,
                    y: 600.0,
                    radius: 30.0,
                    mass: 55_000.0,
                },
            ],
        };
        let observations = vec![
            observation(&truth, (100.0, 300.0), 0.0, 15.0),
            observation(&truth, (100.0, 800.0), -5.0, 15.0),
            observation(&truth, (1_800.0, 300.0), 180.0, 15.0),
            observation(&truth, (1_800.0, 900.0), 175.0, 15.0),
            observation(&truth, (800.0, 100.0), 80.0, 15.0),
            observation(&truth, (800.0, 1_000.0), -80.0, 15.0),
        ];
        let known_first = PlanetMap {
            planets: vec![truth.planets[0].clone()],
        };

        let second =
            seed_planet_from_residuals(&known_first, &observations, world, 30.0, 80.0).unwrap();
        let fit = fit_fixed_count(
            &PlanetMap {
                planets: vec![truth.planets[0].clone(), second],
            },
            &observations,
            world,
        )
        .unwrap();

        assert!((fit.map.planets[1].x - truth.planets[1].x).abs() < 5.0);
        assert!((fit.map.planets[1].y - truth.planets[1].y).abs() < 5.0);
        assert!((fit.map.planets[1].mass - truth.planets[1].mass).abs() < 1_000.0);
    }

    #[test]
    fn greedy_source_count_search_returns_validation_scores_for_each_map() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![
                Planet {
                    x: 500.0,
                    y: 450.0,
                    radius: 30.0,
                    mass: 40_000.0,
                },
                Planet {
                    x: 1_300.0,
                    y: 600.0,
                    radius: 30.0,
                    mass: 55_000.0,
                },
            ],
        };
        let observations = vec![
            observation(&truth, (100.0, 300.0), 0.0, 15.0),
            observation(&truth, (100.0, 800.0), -5.0, 15.0),
            observation(&truth, (1_800.0, 300.0), 180.0, 15.0),
            observation(&truth, (1_800.0, 900.0), 175.0, 15.0),
            observation(&truth, (800.0, 100.0), 80.0, 15.0),
            observation(&truth, (800.0, 1_000.0), -80.0, 15.0),
        ];

        let candidates =
            fit_greedy_source_counts(&observations[..4], &observations[4..], world, 2, 30.0, 80.0)
                .unwrap();

        assert_eq!(candidates.candidates.len(), 2);
        assert_eq!(candidates.candidates[0].result.map.planets.len(), 1);
        assert_eq!(candidates.candidates[1].result.map.planets.len(), 2);
        assert!(candidates.candidates[0].validation_rmse.is_some());
        assert!(
            candidates.candidates[1].validation_rmse.unwrap()
                < candidates.candidates[0].validation_rmse.unwrap()
        );
        assert_eq!(
            candidates.stop_reason,
            SourceCountStopReason::ReachedMaximum
        );
    }

    #[test]
    fn greedy_source_count_search_keeps_lower_counts_when_later_count_lacks_data() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 450.0,
                radius: 30.0,
                mass: 40_000.0,
            }],
        };
        let mut short_shot = observation(&truth, (200.0, 200.0), 25.0, 8.0);
        short_shot.points.truncate(4);

        let search = fit_greedy_source_counts(&[short_shot], &[], world, 2, 30.0, 80.0).unwrap();

        assert_eq!(search.candidates.len(), 1);
        assert_eq!(search.candidates[0].result.map.planets.len(), 1);
        assert_eq!(
            search.stop_reason,
            SourceCountStopReason::InsufficientResiduals {
                required: 6,
                available: 4,
            }
        );
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
