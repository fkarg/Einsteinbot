//! Field-only gravity proposal from an observed trajectory.
use crate::estimator::{
    SeedStrategy, seed_planet_from_residuals_cancellable, seed_planet_from_residuals_with_strategy,
};
use crate::local_field::LocalField;
use crate::map::{PlanetMap, Vec2};
use crate::planet_fit::fit_planet_map;
use crate::protocol::{Planet, Point};
use crate::simulation::{
    ShotObservation, WorldParameters, simulate_sampled_positions,
    simulate_sampled_positions_with_hypothesis, simulate_sampled_positions_with_local_field,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};

/// An explanatory gravity source only: it does not describe a planet or a collision surface.
/// The support path and continuation errors permit measuring evidence proximity, not a
/// calibrated uncertainty or posterior probability. Re-assess against independent paths.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FieldHypothesis {
    pub x: f64,
    pub y: f64,
    pub mass: f64,
    pub support: Vec<Point>,
    /// Independent paths constraining this model; `support` remains for older captures.
    #[serde(default)]
    pub support_paths: Vec<Vec<Point>>,
    /// Legacy field names for mean held-out position RMSE. Single-path proposals score
    /// the held-out tail; joint proposals score whole independent trajectories.
    pub baseline_continuation_rmse: f64,
    pub continuation_rmse: f64,
    /// Independent trajectories used to estimate and validate this source.
    #[serde(default)]
    pub training_paths: usize,
    #[serde(default)]
    pub validation_paths: usize,
    /// Relative reduction in mean held-out position RMSE; heuristic, not a probability.
    #[serde(default)]
    pub relative_improvement: f64,
}

/// Conditional first-path fit, before validation by an independent trajectory.
/// The covariance is an approximate local diagnostic conditional on the single-source
/// model; it excludes missing sources and may omit unidentified directions.
#[derive(Clone, Debug, Serialize)]
pub struct ProvisionalFieldHypothesis {
    pub source: FieldHypothesis,
    pub approximate_covariance: Option<[[f64; 3]; 3]>,
    pub covariance_unavailable_reason: Option<crate::estimator::CovarianceUnavailableReason>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldValidation {
    pub baseline_rmse: f64,
    pub candidate_rmse: f64,
    pub paths: usize,
    pub improved_paths: usize,
    pub regressed: bool,
}

/// Descriptive fit on one observed path, not independent statistical evidence.
#[derive(Clone, Debug)]
pub struct ObservedPathImpact {
    /// Indices into the shot's regular position samples.
    pub sample_indices: Vec<usize>,
    pub baseline_vector_rmse: f64,
    pub candidate_vector_rmse: f64,
}

impl FieldHypothesis {
    /// Select observed samples where this source's acceleration exceeds an
    /// absolute threshold, and compare residual vectors before and after it.
    /// Adjacent samples overlap; they must not be counted as independent paths.
    pub fn observed_path_impact(
        &self,
        baseline: &PlanetMap,
        shot: &ShotObservation,
        threshold: f64,
    ) -> Option<ObservedPathImpact> {
        if !threshold.is_finite() || threshold < 0.0 {
            return None;
        }
        let residuals = crate::estimator::acceleration_residuals(baseline, shot).ok()?;
        let mut indices = Vec::new();
        let (mut before, mut after) = (0.0, 0.0);
        for (index, sample) in residuals.iter().enumerate() {
            let source = self.acceleration(sample.position)?;
            if source.norm_squared().sqrt() < threshold {
                continue;
            }
            let dx = sample.observed.x - sample.predicted.x;
            let dy = sample.observed.y - sample.predicted.y;
            before += dx * dx + dy * dy;
            after += (dx - source.x).powi(2) + (dy - source.y).powi(2);
            indices.push(index + 1);
        }
        if indices.is_empty() {
            return None;
        }
        let count = indices.len() as f64;
        Some(ObservedPathImpact {
            sample_indices: indices,
            baseline_vector_rmse: (before / count).sqrt(),
            candidate_vector_rmse: (after / count).sqrt(),
        })
    }
    /// Distance in world units to the nearest observed timed trajectory segment.
    /// This proxy is not a standard deviation; proximity alone cannot prove accuracy.
    pub fn support_distance(&self, point: Vec2) -> f64 {
        self.support_paths
            .iter()
            .flat_map(|path| path.windows(2))
            .chain(self.support.windows(2))
            .map(|pair| {
                let x = f64::from(pair[0].x) - point.x;
                let y = f64::from(pair[0].y) - point.y;
                let dx = f64::from(pair[1].x) - f64::from(pair[0].x);
                let dy = f64::from(pair[1].y) - f64::from(pair[0].y);
                let length = dx * dx + dy * dy;
                let t = if length == 0.0 {
                    0.0
                } else {
                    (-(x * dx + y * dy) / length).clamp(0.0, 1.0)
                };
                (x + t * dx).hypot(y + t * dy)
            })
            .fold(f64::INFINITY, f64::min)
    }

    /// Relative uncertainty heuristic, not physical covariance or probability.
    /// It starts at 0.5 even on observed paths and rises toward 1 with distance;
    /// the 100-unit distance scale is uncalibrated and is not a confidence bound.
    pub fn relative_uncertainty_proxy(&self, position: Vec2) -> f64 {
        let distance = self.support_distance(position);
        if !distance.is_finite() {
            return 1.0;
        }
        (0.5 + 0.5 * distance / (distance + 100.0)).min(1.0)
    }

    /// Inverse-square acceleration from this field-only point source, if nonsingular.
    pub fn acceleration(&self, position: Vec2) -> Option<Vec2> {
        let dx = self.x - position.x;
        let dy = self.y - position.y;
        let r2 = dx * dx + dy * dy;
        if r2 == 0.0 {
            return None;
        }
        let scale = self.mass / (r2 * r2.sqrt());
        Some(Vec2::new(scale * dx, scale * dy))
    }
}

/// Propose a single-source explanation of a completed path under an empty baseline.
/// Fit only the first 60% of regular samples and validate against the held-out tail.
/// A physical mass/radius prior regularizes fitting, but the output is *field-only*.
/// Cancellation is checked during seeding and passed to the numerical fit.
pub fn propose_field(
    shot: &ShotObservation,
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Option<FieldHypothesis> {
    propose_provisional_field(shot, world, cancelled).map(|proposal| {
        let mut source = proposal.source;
        // Preserve the legacy API's count of its held-out tail. Provisional
        // diagnostics use zero because this tail is not an independent path.
        source.validation_paths = 1;
        source
    })
}

pub fn propose_provisional_field(
    shot: &ShotObservation,
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Option<ProvisionalFieldHypothesis> {
    propose_provisional_field_with_strategy(shot, world, cancelled, SeedStrategy::Grid)
}

/// Offline or per-session variant of the first-flyby proposal.
pub fn propose_provisional_field_with_strategy(
    shot: &ShotObservation,
    world: WorldParameters,
    cancelled: &AtomicBool,
    strategy: SeedStrategy,
) -> Option<ProvisionalFieldHypothesis> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let count = shot.points.len().checked_sub(1)?;
    if count < 20 {
        return None;
    }
    let split = count * 3 / 5;
    let mut early = shot.clone();
    // Both seeder and fitter omit the untimed final point. Including one extra regular
    // point leaves [0, split) for training and [split, count) exclusively for validation.
    early.points.truncate(split + 1);
    let flat = PlanetMap { planets: vec![] };
    let seed = seed_planet_from_residuals_with_strategy(
        &flat,
        std::slice::from_ref(&early),
        world,
        30.0,
        50.0,
        strategy,
        cancelled,
    )
    .ok()?;
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let radius = (10.0 * seed.mass).cbrt().clamp(20.0, 40.0);
    let initial = PlanetMap {
        planets: vec![Planet {
            radius,
            mass: radius.powi(3) / 10.0,
            ..seed
        }],
    };
    let fit = fit_planet_map(&initial, &[early], &[], world, cancelled).ok()?;
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let flat_path = simulate_sampled_positions(&flat, shot, world, count).ok()?;
    let fitted_path = simulate_sampled_positions(&fit.map, shot, world, count).ok()?;
    let rmse = |path: &[Vec2]| -> f64 {
        (shot.points[split..count]
            .iter()
            .zip(&path[split..])
            .map(|(p, q)| (f64::from(p.x) - q.x).powi(2) + (f64::from(p.y) - q.y).powi(2))
            .sum::<f64>()
            / (2 * (count - split)) as f64)
            .sqrt()
    };
    let baseline_continuation_rmse = rmse(&flat_path);
    let continuation_rmse = rmse(&fitted_path);
    // A 0.01-unit noise floor and a minimum 20% improvement are heuristics, not statistical
    // confidence. They reject tiny changes caused by f32 quantization in flat controls.
    if baseline_continuation_rmse <= 0.01
        || continuation_rmse >= baseline_continuation_rmse * 0.8
        || cancelled.load(Ordering::Relaxed)
    {
        return None;
    }
    let source = &fit.map.planets[0];
    let covariance = fit
        .approximate_covariance
        .as_ref()
        .or(fit.covariance.as_ref())
        .map(|matrix| std::array::from_fn(|row| std::array::from_fn(|col| matrix[(row, col)])));
    Some(ProvisionalFieldHypothesis {
        approximate_covariance: covariance,
        covariance_unavailable_reason: fit.covariance_unavailable_reason,
        source: FieldHypothesis {
            x: source.x,
            y: source.y,
            mass: source.mass,
            support: shot.points[..count].to_vec(),
            support_paths: vec![shot.points[..count].to_vec()],
            baseline_continuation_rmse,
            continuation_rmse,
            training_paths: 1,
            validation_paths: 0,
            relative_improvement: (1.0 - continuation_rmse / baseline_continuation_rmse)
                .clamp(0.0, 1.0),
        },
    })
}

/// Propose an additional gravity-only source against an unchanged physical map.
/// Fit a free-mass source to the first 60% of timed samples and validate its
/// continuation on the remaining samples. The physical planets are never fitted
/// or assigned new contact geometry. This check is heuristic, not a probability.
/// Cancellation is checked during the residual grid search.
pub fn propose_residual_field(
    shot: &ShotObservation,
    baseline: &PlanetMap,
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Option<FieldHypothesis> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let count = shot.points.len().checked_sub(1)?;
    if count < 20 {
        return None;
    }
    let split = count * 3 / 5;
    let mut early = shot.clone();
    // The seeder omits its final point; all continuation points are withheld.
    early.points.truncate(split + 1);
    let source =
        seed_planet_from_residuals_cancellable(baseline, &[early], world, 30.0, 50.0, cancelled)
            .ok()?;
    if cancelled.load(Ordering::Relaxed)
        || ![source.x, source.y, source.mass]
            .iter()
            .all(|x| x.is_finite())
        || source.mass <= 0.0
    {
        return None;
    }
    let mut alternative = baseline.clone();
    alternative.planets.push(Planet {
        radius: 0.0,
        ..source.clone()
    });
    let base_path = simulate_sampled_positions(baseline, shot, world, count).ok()?;
    let proposed_path = simulate_sampled_positions(&alternative, shot, world, count).ok()?;
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let rmse = |path: &[Vec2]| -> f64 {
        (shot.points[split..count]
            .iter()
            .zip(&path[split..])
            .map(|(p, q)| (f64::from(p.x) - q.x).powi(2) + (f64::from(p.y) - q.y).powi(2))
            .sum::<f64>()
            / (2 * (count - split)) as f64)
            .sqrt()
    };
    let baseline_continuation_rmse = rmse(&base_path);
    let continuation_rmse = rmse(&proposed_path);
    if !baseline_continuation_rmse.is_finite()
        || !continuation_rmse.is_finite()
        || baseline_continuation_rmse <= 0.01
        || continuation_rmse >= baseline_continuation_rmse * 0.8
    {
        return None;
    }
    Some(FieldHypothesis {
        x: source.x,
        y: source.y,
        mass: source.mass,
        support: shot.points[..count].to_vec(),
        support_paths: vec![shot.points[..count].to_vec()],
        baseline_continuation_rmse,
        continuation_rmse,
        training_paths: 1,
        validation_paths: 1,
        relative_improvement: (1.0 - continuation_rmse / baseline_continuation_rmse)
            .clamp(0.0, 1.0),
    })
}

/// Seed a field-only source from prior paths and require improvement on distinct whole paths.
/// The source extends globally as a gravity field, but only the local residual is interpolated
/// near training paths. Neither component supplies collision geometry.
pub fn propose_joint_residual_field(
    training: &[ShotObservation],
    validation: &[ShotObservation],
    baseline: &PlanetMap,
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Option<FieldHypothesis> {
    propose_joint_residual_field_with_strategy(
        training,
        validation,
        baseline,
        world,
        cancelled,
        SeedStrategy::Grid,
    )
}

pub(crate) fn propose_joint_residual_field_with_strategy(
    training: &[ShotObservation],
    validation: &[ShotObservation],
    baseline: &PlanetMap,
    world: WorldParameters,
    cancelled: &AtomicBool,
    strategy: SeedStrategy,
) -> Option<FieldHypothesis> {
    if cancelled.load(Ordering::Relaxed) {
        return None;
    }
    let training = distinct_paths(training);
    let validation = distinct_paths(validation);
    if training.is_empty() || validation.is_empty() {
        return None;
    }
    let source = seed_planet_from_residuals_with_strategy(
        baseline, &training, world, 30.0, 50.0, strategy, cancelled,
    )
    .ok()?;
    if cancelled.load(Ordering::Relaxed)
        || ![source.x, source.y, source.mass]
            .iter()
            .all(|value| value.is_finite())
        || source.mass <= 0.0
    {
        return None;
    }

    let candidate = FieldHypothesis {
        x: source.x,
        y: source.y,
        mass: source.mass,
        support: training[0].points.clone(),
        support_paths: training
            .iter()
            .chain(&validation)
            .map(|shot| shot.points.clone())
            .collect(),
        baseline_continuation_rmse: 0.0,
        continuation_rmse: 0.0,
        training_paths: training.len(),
        validation_paths: 0,
        relative_improvement: 0.0,
    };

    let Ok(Some(validation)) = validate_residual_field(
        &candidate,
        &training,
        &validation,
        baseline,
        world,
        cancelled,
    ) else {
        return None;
    };
    if validation.regressed || validation.improved_paths == 0 || cancelled.load(Ordering::Relaxed) {
        return None;
    }
    Some(FieldHypothesis {
        baseline_continuation_rmse: validation.baseline_rmse,
        continuation_rmse: validation.candidate_rmse,
        validation_paths: validation.paths,
        relative_improvement: if validation.baseline_rmse > 0.0 {
            (1.0 - validation.candidate_rmse / validation.baseline_rmse).clamp(0.0, 1.0)
        } else {
            0.0
        },
        ..candidate
    })
}

/// Compare source/no-source predictions on whole paths not used to build either residual field.
/// `regressed` flags any path whose error worsens by more than 20% plus the quantization floor.
pub fn validate_residual_field(
    hypothesis: &FieldHypothesis,
    training: &[ShotObservation],
    validation: &[ShotObservation],
    baseline: &PlanetMap,
    world: WorldParameters,
    cancelled: &AtomicBool,
) -> Result<Option<FieldValidation>, String> {
    let training = distinct_paths(training);
    let validation = distinct_paths(validation);
    if training.is_empty() || validation.is_empty() {
        return Ok(None);
    }
    let baseline_field = LocalField::from_observations(baseline, &training);
    let source_field = LocalField::from_observations(
        &PlanetMap {
            planets: baseline
                .planets
                .iter()
                .cloned()
                .chain(std::iter::once(Planet {
                    x: hypothesis.x,
                    y: hypothesis.y,
                    radius: 0.0,
                    mass: hypothesis.mass,
                }))
                .collect(),
        },
        &training,
    );
    let mut baseline_error = 0.0;
    let mut candidate_error = 0.0;
    let mut paths = 0;
    let mut improved_paths = 0;
    let mut regressed = false;
    for shot in &validation {
        if cancelled.load(Ordering::Relaxed) {
            return Err("cancelled".to_owned());
        }
        let Some(base) = path_rmse(baseline, &baseline_field, None, shot, world)? else {
            continue;
        };
        let Some(candidate) = path_rmse(baseline, &source_field, Some(hypothesis), shot, world)?
        else {
            return Ok(None);
        };
        regressed |= candidate > base * 1.2 + 0.01;
        if base > 0.01 && candidate < base * 0.8 {
            improved_paths += 1;
        }
        baseline_error += base;
        candidate_error += candidate;
        paths += 1;
    }
    Ok((paths > 0).then(|| FieldValidation {
        baseline_rmse: baseline_error / paths as f64,
        candidate_rmse: candidate_error / paths as f64,
        paths,
        improved_paths,
        regressed,
    }))
}

fn distinct_paths(shots: &[ShotObservation]) -> Vec<ShotObservation> {
    let mut distinct = Vec::new();
    for shot in shots.iter().filter(|shot| shot.points.len() >= 3) {
        if !distinct.iter().any(|known: &ShotObservation| {
            known.angle_degrees == shot.angle_degrees
                && known.velocity == shot.velocity
                && known.points == shot.points
        }) {
            distinct.push(shot.clone());
        }
    }
    distinct
}

fn path_rmse(
    map: &PlanetMap,
    field: &LocalField,
    hypothesis: Option<&FieldHypothesis>,
    shot: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<f64>, String> {
    let Some(sample_count) = shot.points.len().checked_sub(1) else {
        return Ok(None);
    };
    if sample_count < 2 {
        return Ok(None);
    }
    let predicted = if let Some(hypothesis) = hypothesis {
        simulate_sampled_positions_with_hypothesis(
            map,
            field,
            hypothesis,
            shot,
            world,
            sample_count,
        )
    } else {
        simulate_sampled_positions_with_local_field(map, field, shot, world, sample_count)
    }?;
    let mut squared = 0.0;
    let mut coordinates = 0;
    for (observed, predicted) in shot.points[1..sample_count].iter().zip(&predicted[1..]) {
        squared += (f64::from(observed.x) - predicted.x).powi(2)
            + (f64::from(observed.y) - predicted.y).powi(2);
        coordinates += 2;
    }
    Ok((coordinates > 0).then(|| (squared / coordinates as f64).sqrt()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::shot_position_rmse;
    use crate::map::PlanetMap;
    use crate::protocol::{Planet, Point};
    use crate::simulation::{ShotObservation, Termination, WorldParameters, simulate_shot};
    use std::sync::atomic::AtomicBool;

    fn observed(map: &PlanetMap, y: f32, angle: f64) -> ShotObservation {
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: angle,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y }],
        };
        let path = simulate_shot(map, &shot, WorldParameters::default(), 2000).unwrap();
        assert_eq!(path.termination, Termination::BattlefieldMargin);
        shot.points = path.points;
        shot
    }

    fn observed_any(map: &PlanetMap, y: f32, angle: f64) -> ShotObservation {
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: angle,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y }],
        };
        shot.points = simulate_shot(map, &shot, WorldParameters::default(), 2000)
            .unwrap()
            .points;
        shot
    }
    #[test]
    fn straight_quantized_path_does_not_propose_gravity() {
        let shot = observed(&PlanetMap { planets: vec![] }, 400.0, -7.0);
        assert!(
            propose_field(&shot, WorldParameters::default(), &AtomicBool::new(false)).is_none()
        );
    }
    #[test]
    fn flyby_supports_field_but_does_not_create_obstacle() {
        let world = WorldParameters::default();
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.,
                y: 500.,
                radius: 30.,
                mass: 2700.,
            }],
        };
        let shot = observed(&truth, 400.0, 0.0);
        let proposal = propose_field(&shot, world, &AtomicBool::new(false)).expect("curved flyby");
        // A field proposal exposes only source strength/position and has no radius or collision surface.
        let _ = (proposal.x, proposal.y, proposal.mass);
        let field = PlanetMap {
            planets: vec![Planet {
                x: proposal.x,
                y: proposal.y,
                radius: 0.0,
                mass: proposal.mass,
            }],
        };
        let heldout = observed(&truth, 350.0, 0.0);
        let learned = shot_position_rmse(&field, &heldout, world)
            .unwrap()
            .unwrap();
        let baseline = shot_position_rmse(&PlanetMap { planets: vec![] }, &heldout, world)
            .unwrap()
            .unwrap();
        assert!(
            learned < baseline * 0.1,
            "learned={learned} flat={baseline}"
        );
        let on_path = proposal.support_distance(crate::map::Vec2::new(200.0, 400.0));
        let far = proposal.support_distance(crate::map::Vec2::new(200.0, 50.0));
        assert!(
            on_path < 0.001 && far > on_path + 100.0,
            "on_path={on_path} far={far}"
        );
    }
    #[test]
    fn impact_query_reports_field_strength_and_residual_reduction_on_observed_samples() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let shot = observed(&truth, 400.0, 0.0);
        let hypothesis =
            propose_field(&shot, WorldParameters::default(), &AtomicBool::new(false)).unwrap();
        let impact = hypothesis
            .observed_path_impact(&PlanetMap { planets: vec![] }, &shot, 0.01)
            .unwrap();
        assert!(!impact.sample_indices.is_empty());
        assert!(impact.candidate_vector_rmse < impact.baseline_vector_rmse * 0.05);
        assert!(
            hypothesis
                .observed_path_impact(&PlanetMap { planets: vec![] }, &shot, 1000.0)
                .is_none()
        );
    }
    #[test]
    fn uncertainty_proxy_is_high_at_support_and_increases_with_distance() {
        let hypothesis = FieldHypothesis {
            x: 20.0,
            y: 10.0,
            mass: 2700.0,
            support: vec![Point { x: 0.0, y: 0.0 }, Point { x: 100.0, y: 0.0 }],
            support_paths: vec![],
            baseline_continuation_rmse: 10.0,
            continuation_rmse: 1.0,
            training_paths: 1,
            validation_paths: 1,
            relative_improvement: 0.9,
        };
        let on = hypothesis.relative_uncertainty_proxy(Vec2::new(50.0, 0.0));
        let near = hypothesis.relative_uncertainty_proxy(Vec2::new(50.0, 25.0));
        let far = hypothesis.relative_uncertainty_proxy(Vec2::new(50.0, 200.0));
        assert!(
            on >= 0.5 && on < near && near < far && far <= 1.0,
            "on={on} near={near} far={far}"
        );
    }

    #[test]
    fn partial_baseline_does_not_propose_a_source_for_explained_gravity() {
        let known = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let train = observed(&known, 400.0, 0.0);
        assert!(
            propose_residual_field(
                &train,
                &known,
                WorldParameters::default(),
                &AtomicBool::new(false)
            )
            .is_none()
        );
    }

    #[test]
    fn partial_baseline_proposes_field_that_improves_an_independent_path() {
        let known = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let original = known.clone();
        let truth = PlanetMap {
            planets: vec![
                known.planets[0].clone(),
                Planet {
                    x: 1000.0,
                    y: 350.0,
                    radius: 35.0,
                    mass: 4287.5,
                },
            ],
        };
        let train = observed(&truth, 400.0, 0.0);
        let proposal = propose_residual_field(
            &train,
            &known,
            WorldParameters::default(),
            &AtomicBool::new(false),
        )
        .expect("unexplained gravity");
        assert_eq!(known, original);
        assert!(proposal.continuation_rmse < proposal.baseline_continuation_rmse * 0.8);
        let mut heldout = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 350.0 }],
        };
        heldout.points = simulate_shot(&truth, &heldout, WorldParameters::default(), 2000)
            .unwrap()
            .points;
        let mut alternative = known.clone();
        alternative.planets.push(Planet {
            x: proposal.x,
            y: proposal.y,
            radius: 0.0,
            mass: proposal.mass,
        });
        let baseline = shot_position_rmse(&known, &heldout, WorldParameters::default())
            .unwrap()
            .unwrap();
        let improved = shot_position_rmse(&alternative, &heldout, WorldParameters::default())
            .unwrap()
            .unwrap();
        assert!(
            improved < baseline * 0.5,
            "baseline={baseline} improved={improved}"
        );
        // Crossing within a hypothetical planet radius must not create a collision.
        let close = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 50.0,
            points: vec![Point {
                x: (proposal.x - 12.0) as f32,
                y: (proposal.y + 15.0) as f32,
            }],
        };
        let path = crate::simulation::simulate_targeting_shot_with_hypothesis(
            &known,
            &crate::local_field::LocalField::default(),
            &proposal,
            &close,
            WorldParameters::default(),
            2,
            &[],
            4.0,
        )
        .unwrap();
        assert_eq!(path.termination, Termination::SampleLimit);
    }

    #[test]
    fn joint_residual_source_uses_prior_paths_and_improves_a_whole_heldout_path() {
        let known = PlanetMap {
            planets: vec![Planet {
                x: 600.0,
                y: 500.0,
                radius: 30.0,
                mass: 2700.0,
            }],
        };
        let truth = PlanetMap {
            planets: vec![
                known.planets[0].clone(),
                Planet {
                    x: 900.0,
                    y: 500.0,
                    radius: 35.0,
                    mass: 4287.5,
                },
            ],
        };
        let training = [
            observed_any(&truth, 400.0, 0.0),
            observed_any(&truth, 350.0, 0.0),
        ];
        let validation = [observed_any(&truth, 250.0, 0.0)];
        let proposal = propose_joint_residual_field(
            &training,
            &validation,
            &known,
            WorldParameters::default(),
            &AtomicBool::new(false),
        )
        .expect("joint residual source predicts a separate path");

        assert_eq!(proposal.training_paths, 2);
        assert_eq!(proposal.validation_paths, 1);
        assert!(proposal.relative_improvement >= 0.2);
        let on_path = proposal.support_distance(Vec2::new(200.0, 400.0));
        let other_path = proposal.support_distance(Vec2::new(200.0, 350.0));
        let far = proposal.support_distance(Vec2::new(200.0, 50.0));
        assert!(on_path < 0.001 && other_path < 0.001 && far > on_path + 100.0);

        let mut invalid_world = WorldParameters::default();
        invalid_world.substeps_per_tick = 0;
        assert!(
            validate_residual_field(
                &proposal,
                &training,
                &validation,
                &known,
                invalid_world,
                &AtomicBool::new(false),
            )
            .is_err()
        );
    }

    #[test]
    fn pre_cancelled_does_not_propose() {
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 600.,
                y: 500.,
                radius: 30.,
                mass: 2700.,
            }],
        };
        let shot = observed(&truth, 400., 0.);
        assert!(propose_field(&shot, WorldParameters::default(), &AtomicBool::new(true)).is_none());
    }
}
