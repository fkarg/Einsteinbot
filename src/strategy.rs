use crate::coverage::path_cells;
use crate::map::{PlanetMap, Vec2};
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_targeting_shot,
    simulate_targeting_shot_with_hypothesis, simulate_targeting_shot_with_local_field,
};
use nalgebra::DMatrix;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct MapBelief {
    pub map: PlanetMap,
    pub local_field: crate::local_field::LocalField,
    /// An uncertain field-only explanation, never a collision obstacle.
    pub field_hypothesis: Option<crate::field_hypothesis::FieldHypothesis>,
    /// Local residuals recomputed against the physical field plus that hypothesis.
    pub hypothesis_local_field: Option<crate::local_field::LocalField>,
    /// Covariance ordered by `[center_x, center_y, mass]` for each planet.
    pub parameter_covariance: Option<DMatrix<f64>>,
    /// Observed contact points, retained even before radius is identifiable.
    pub contact_points: Vec<Point>,
    /// Raw trajectory error exposes model mismatch beyond local covariance.
    pub model_position_rmse: Option<f64>,
    pub fit_converged: Option<bool>,
    pub planet_contact_counts: Vec<usize>,
    pub uncertain_associations: usize,
}

impl MapBelief {
    pub fn new(map: PlanetMap, parameter_covariance: Option<DMatrix<f64>>) -> Self {
        Self {
            map,
            local_field: crate::local_field::LocalField::default(),
            field_hypothesis: None,
            hypothesis_local_field: None,
            parameter_covariance,
            contact_points: Vec::new(),
            model_position_rmse: None,
            fit_converged: None,
            planet_contact_counts: Vec::new(),
            uncertain_associations: 0,
        }
    }

    /// Gravity disagreement between the supported source and no-source models.
    /// This is a model-choice spread heuristic, not a probability or full epistemic
    /// uncertainty; it is unavailable without a validated field hypothesis.
    pub fn model_disagreement_at(&self, position: Vec2) -> Option<f64> {
        // Treat the validated source and the no-source model as alternatives. Their
        // gravity difference is broad in space, while the validation score controls
        // how much model-choice uncertainty remains. This is a heuristic mixture, not
        // a calibrated posterior; keep it separate from physical parameter covariance.
        self.field_hypothesis.as_ref().and_then(|hypothesis| {
            let evidence = 1.0
                - (-hypothesis.relative_improvement.clamp(0.0, 1.0)
                    * hypothesis.validation_paths as f64)
                    .exp();
            let base = self
                .local_field
                .residual(position)
                .unwrap_or_else(|| Vec2::new(0.0, 0.0));
            let source = hypothesis.acceleration(position)?;
            let correction = self
                .hypothesis_local_field
                .as_ref()
                .and_then(|field| field.residual(position))
                .unwrap_or_else(|| Vec2::new(0.0, 0.0));
            let difference =
                (source.x + correction.x - base.x).hypot(source.y + correction.y - base.y);
            Some((evidence * (1.0 - evidence)).sqrt() * difference)
        })
    }

    fn field_uncertainty(&self, position: Vec2) -> Option<f64> {
        let parameter = self.parameter_covariance.as_ref().and_then(|parameters| {
            self.map
                .acceleration_covariance(position, parameters)
                .ok()
                .map(|covariance| (covariance.xx + covariance.yy).max(0.0).sqrt())
        });
        let model = self.model_disagreement_at(position).map(f64::abs);
        match (parameter, model) {
            (Some(parameter), Some(model)) => Some(parameter.max(model)),
            (Some(uncertainty), None) | (None, Some(uncertainty)) => Some(uncertainty),
            (None, None) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct KnownShot {
    launch: Point,
    velocity: f64,
    angle_degrees: f64,
}

/// A shot that began but has not finished yet. `launch` is the player's tracked position when
/// the begin event arrived (`None` when unknown); it never constrains candidates from elsewhere.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct InFlightShot {
    player_id: u32,
    launch: Option<Point>,
    velocity: f64,
    angle_degrees: f64,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct BotState {
    own_id: Option<u32>,
    player_positions: BTreeMap<u32, Point>,
    #[serde(skip)]
    map_belief: Option<MapBelief>,
    known_shots: Vec<KnownShot>,
    covered_cells: BTreeSet<(i32, i32)>,
    in_flight: Vec<InFlightShot>,
}

impl BotState {
    /// Restore the bot's captured targeting state for offline inspection and replay.
    pub fn from_diagnostic_snapshot(
        snapshot: &serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        serde_json::from_value(snapshot.clone())
    }

    pub fn observe(&mut self, event: &ServerEvent) {
        match event {
            ServerEvent::Planets { .. } => {
                self.known_shots.clear();
                self.covered_cells.clear();
                self.map_belief = None;
                self.in_flight.clear();
            }
            ServerEvent::OwnId { player_id } => self.own_id = Some(*player_id),
            ServerEvent::PlayerPosition { player_id, x, y } => {
                self.player_positions
                    .insert(*player_id, Point { x: *x, y: *y });
            }
            ServerEvent::PlayerLeave { player_id } => {
                self.player_positions.remove(player_id);
                self.in_flight.retain(|shot| shot.player_id != *player_id);
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => {
                // Every connection receives every ShotBegin broadcast. Shots take seconds to
                // finish, so in-flight own shots must already count as known outcomes.
                self.in_flight.push(InFlightShot {
                    player_id: *player_id,
                    launch: self.player_positions.get(player_id).cloned(),
                    velocity: *velocity,
                    angle_degrees: *angle,
                });
            }
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                points,
            } => {
                if let Some(index) = self.in_flight.iter().position(|shot| {
                    shot.player_id == *player_id
                        && shot.velocity == *velocity
                        && shot.angle_degrees == *angle
                }) {
                    self.in_flight.swap_remove(index);
                }
                // Every connection receives every ShotFin broadcast, so each bot sees all
                // completed trajectories and remembers their exact inputs and launch point.
                self.covered_cells
                    .extend(path_cells(&points[..points.len().saturating_sub(1)]));
                if let Some(launch) = points.first() {
                    self.known_shots.push(KnownShot {
                        launch: launch.clone(),
                        velocity: *velocity,
                        angle_degrees: *angle,
                    });
                }
            }
            _ => {}
        }
    }

    /// Whether a trajectory with exactly this launch point, velocity and angle is already known
    /// — either completed or still in flight. Launch coordinates match exactly because the
    /// trajectory's first point, `PlayerPosition`, and the launch captured at `ShotBegin` are
    /// the same f32 rounding of the server's launch position.
    pub fn is_known_shot(&self, launch: &Point, velocity: i32, angle_degrees: i32) -> bool {
        let velocity = f64::from(velocity);
        let angle_degrees = f64::from(angle_degrees);
        self.known_shots.iter().any(|shot| {
            shot.launch.x == launch.x
                && shot.launch.y == launch.y
                && shot.velocity == velocity
                && shot.angle_degrees == angle_degrees
        }) || self.in_flight.iter().any(|shot| {
            shot.launch.as_ref().is_some_and(|known| known == launch)
                && shot.velocity == velocity
                && shot.angle_degrees == angle_degrees
        })
    }

    pub fn diagnostic_snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "own_id": self.own_id,
            "player_positions": self.player_positions,
            "known_shots": self.known_shots,
            "in_flight": self.in_flight,
            "covered_cells": self.covered_cells,
        })
    }

    pub fn player_positions(&self) -> &BTreeMap<u32, Point> {
        &self.player_positions
    }

    pub fn own_id(&self) -> Option<u32> {
        self.own_id
    }

    pub fn set_map_belief(&mut self, map_belief: Option<MapBelief>) {
        self.map_belief = map_belief;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TargetingSettings {
    pub angle_step_degrees: u16,
    pub max_simulation_ticks: usize,
    pub target_radius: f64,
    /// Average acceleration standard deviation required before probing is considered.
    pub information_threshold: f64,
    pub information_weight: f64,
    /// Maximum approach-distance penalty for retracing observed space (world units).
    pub coverage_penalty: f64,
}

impl Default for TargetingSettings {
    fn default() -> Self {
        Self {
            angle_step_degrees: 10,
            max_simulation_ticks: 180,
            target_radius: 4.0,
            information_threshold: 0.05,
            information_weight: 1.0,
            coverage_penalty: 40.0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShotReason {
    Hit,
    Information,
    Approach,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShotChoice {
    pub velocity: i32,
    pub angle_degrees: i32,
    pub miss_distance: f64,
    pub information_score: f64,
    pub uncertainty_available: bool,
    pub covered_fraction: f64,
    /// A provisional contact disk intersects this path before its closest target passage.
    pub contact_before_target: bool,
    pub reason: ShotReason,
    pub predicted_hit: bool,
    pub used_field_hypothesis: bool,
    pub predicted_victim: Option<u32>,
    /// Whether the no-hypothesis alternative predicts the same hit; None when untested.
    pub alternative_agrees: Option<bool>,
    /// Largest relative uncertainty proxy along the predicted path; uncalibrated.
    pub hypothesis_uncertainty_proxy: Option<f64>,
    /// Fraction of sampled trajectory near observed residual gravity, when available.
    pub local_support_fraction: Option<f64>,
}

pub struct TargetingPolicy {
    settings: TargetingSettings,
    world: WorldParameters,
}

impl Default for TargetingPolicy {
    fn default() -> Self {
        Self::new(TargetingSettings::default())
    }
}

impl TargetingPolicy {
    pub fn new(settings: TargetingSettings) -> Self {
        Self {
            settings,
            world: WorldParameters::default(),
        }
    }

    pub fn choose(&self, state: &BotState, energy: f64) -> Option<ShotChoice> {
        self.choose_with_check(state, energy, || true)
    }

    pub fn choose_while_running(
        &self,
        state: &BotState,
        energy: f64,
        running: &std::sync::atomic::AtomicBool,
    ) -> Option<ShotChoice> {
        self.choose_with_check(state, energy, || {
            running.load(std::sync::atomic::Ordering::Relaxed)
        })
    }

    fn choose_with_check(
        &self,
        state: &BotState,
        energy: f64,
        mut running: impl FnMut() -> bool,
    ) -> Option<ShotChoice> {
        let own_id = state.own_id?;
        let start = state.player_positions.get(&own_id)?;
        let targets = state
            .player_positions
            .iter()
            .filter(|(player_id, _)| **player_id != own_id)
            .map(|(_, position)| Vec2::new(f64::from(position.x), f64::from(position.y)))
            .collect::<Vec<_>>();
        if targets.is_empty() || !energy.is_finite() || energy < 2.0 {
            return None;
        }

        let max_velocity = energy.floor().min(50.0) as i32;
        let empty_map = PlanetMap { planets: vec![] };
        let map = state
            .map_belief
            .as_ref()
            .map(|belief| &belief.map)
            .unwrap_or(&empty_map);
        let covariance_available = state.map_belief.as_ref().is_some_and(|belief| {
            belief
                .parameter_covariance
                .as_ref()
                .is_some_and(|covariance| {
                    covariance.nrows() == belief.map.planets.len() * 3
                        && covariance.ncols() == belief.map.planets.len() * 3
                })
        });
        let angle_step = usize::from(self.settings.angle_step_degrees.max(1));
        let players = state
            .player_positions
            .iter()
            .map(|(&id, position)| (id, Vec2::new(f64::from(position.x), f64::from(position.y))))
            .collect::<Vec<_>>();
        let evaluate = |velocity: i32, angle: i32, use_hypothesis: bool| -> Option<ShotChoice> {
            let shot = ShotObservation {
                player_id: own_id,
                angle_degrees: f64::from(angle),
                velocity: f64::from(velocity),
                points: vec![start.clone()],
            };
            let result = if let Some(belief) = &state.map_belief {
                if let (true, Some(hypothesis), Some(field)) = (
                    use_hypothesis,
                    &belief.field_hypothesis,
                    &belief.hypothesis_local_field,
                ) {
                    simulate_targeting_shot_with_hypothesis(
                        map,
                        field,
                        hypothesis,
                        &shot,
                        self.world,
                        self.settings.max_simulation_ticks.max(2),
                        &players,
                        self.settings.target_radius,
                    )
                } else {
                    simulate_targeting_shot_with_local_field(
                        map,
                        &belief.local_field,
                        &shot,
                        self.world,
                        self.settings.max_simulation_ticks.max(2),
                        &players,
                        self.settings.target_radius,
                    )
                }
            } else {
                simulate_targeting_shot(
                    map,
                    &shot,
                    self.world,
                    self.settings.max_simulation_ticks.max(2),
                    &players,
                    self.settings.target_radius,
                )
            };
            let Ok(trajectory) = result else {
                return None;
            };
            let (miss_distance, contact_before_target) = targets
                .iter()
                .map(|target| {
                    let (miss, passage) = closest_path_passage(
                        target,
                        &trajectory.points,
                        self.settings.target_radius,
                    );
                    let blocked =
                        map.planets.iter().any(|planet| {
                            contact_before_target(planet, &trajectory.points, passage)
                        }) || state.map_belief.as_ref().is_some_and(|belief| {
                            belief.contact_points.iter().any(|contact| {
                                first_circle_entry(
                                    &Vec2::new(f64::from(contact.x), f64::from(contact.y)),
                                    5.0,
                                    &trajectory.points,
                                )
                                .is_some_and(|entry| entry < passage)
                            })
                        });
                    (miss, blocked)
                })
                .min_by(|left, right| left.0.total_cmp(&right.0))
                .unwrap();
            let cells = path_cells(&trajectory.points);
            let covered_fraction = if cells.is_empty() {
                1.0
            } else {
                cells.intersection(&state.covered_cells).count() as f64 / cells.len() as f64
            };
            let predicted_hit =
                matches!(trajectory.termination, Termination::PlayerCollision(id) if id != own_id);
            let miss_distance = match trajectory.termination {
                Termination::PlayerCollision(id) if id == own_id => 1.0e9,
                Termination::PlayerCollision(id) => {
                    // Estimate the impact margin from the incoming direction, rather than
                    // treating every first entry into the player radius as equally central.
                    let target = &state.player_positions[&id];
                    let points = &trajectory.points;
                    let end = &points[points.len() - 1];
                    let previous = &points[points.len() - 2];
                    let dx = f64::from(end.x - previous.x);
                    let dy = f64::from(end.y - previous.y);
                    let length = dx.hypot(dy);
                    if length == 0.0 {
                        miss_distance
                    } else {
                        let tx = f64::from(target.x - end.x);
                        let ty = f64::from(target.y - end.y);
                        (tx * dy - ty * dx).abs() / length
                    }
                }
                _ => miss_distance,
            };
            let local_support_fraction = state.map_belief.as_ref().and_then(|belief| {
                (!belief.local_field.is_empty()).then(|| {
                    trajectory
                        .points
                        .iter()
                        .filter(|point| {
                            belief
                                .local_field
                                .residual(Vec2::new(f64::from(point.x), f64::from(point.y)))
                                .is_some()
                        })
                        .count() as f64
                        / trajectory.points.len() as f64
                })
            });
            Some(ShotChoice {
                velocity,
                angle_degrees: angle,
                miss_distance,
                information_score: 0.0,
                uncertainty_available: covariance_available,
                covered_fraction,
                contact_before_target,
                reason: ShotReason::Approach,
                predicted_hit,
                used_field_hypothesis: use_hypothesis,
                predicted_victim: match trajectory.termination {
                    Termination::PlayerCollision(id) if id != own_id => Some(id),
                    _ => None,
                },
                alternative_agrees: None,
                hypothesis_uncertainty_proxy: None,
                local_support_fraction,
            })
        };
        let mut candidates = Vec::new();
        for velocity in 2..=max_velocity {
            let mut angles = (-180..=180).step_by(angle_step).collect::<Vec<_>>();
            if angles.last() != Some(&180) {
                angles.push(180);
            }
            for angle in angles {
                for use_hypothesis in [false, true] {
                    if !running() {
                        return None;
                    }
                    if use_hypothesis
                        && !state.map_belief.as_ref().is_some_and(|belief| {
                            belief.field_hypothesis.is_some()
                                && belief.hypothesis_local_field.is_some()
                        })
                    {
                        continue;
                    }
                    if let Some(candidate) = evaluate(velocity, angle, use_hypothesis) {
                        candidates.push(candidate);
                    }
                }
            }
        }
        // Refine several promising basins; the coarse scan still covers the full energy range.
        let mut seeds = candidates
            .iter()
            .filter(|candidate| {
                !state.is_known_shot(start, candidate.velocity, candidate.angle_degrees)
            })
            .copied()
            .collect::<Vec<_>>();
        if seeds.is_empty() {
            seeds = candidates.clone();
        }
        seeds.sort_by(|a, b| a.miss_distance.total_cmp(&b.miss_distance));
        let mut sampled = candidates
            .iter()
            .map(|c| (c.velocity, c.angle_degrees, c.used_field_hypothesis))
            .collect::<BTreeSet<_>>();
        let mut seed_counts = BTreeMap::new();
        let mut refined_per_model = BTreeMap::new();
        for seed in seeds.into_iter().filter(|seed| {
            let count = seed_counts
                .entry((seed.angle_degrees, seed.used_field_hypothesis))
                .or_insert(0);
            *count += 1;
            if *count > 2 {
                return false;
            }
            let refined = refined_per_model
                .entry(seed.used_field_hypothesis)
                .or_insert(0);
            *refined += 1;
            *refined <= 12
        }) {
            for angle in (seed.angle_degrees - angle_step as i32 + 1)
                ..(seed.angle_degrees + angle_step as i32)
            {
                if !running() {
                    return None;
                }
                let angle = (angle + 180).rem_euclid(360) - 180;
                if sampled.insert((seed.velocity, angle, seed.used_field_hypothesis)) {
                    if let Some(candidate) =
                        evaluate(seed.velocity, angle, seed.used_field_hypothesis)
                    {
                        candidates.push(candidate);
                    }
                }
            }
        }
        if candidates.is_empty() {
            return None;
        }

        // Deliberately repeating an already-observed shot supplies no new field information,
        // so rank only candidates with unknown outcomes when any exist. If every legal
        // candidate is known, fall back to the full pool rather than skipping the shot.
        let start_point = start.clone();
        let mut pool: Vec<ShotChoice> = candidates
            .iter()
            .filter(|candidate| {
                !state.is_known_shot(&start_point, candidate.velocity, candidate.angle_degrees)
            })
            .copied()
            .collect();
        if pool.is_empty() {
            pool = candidates.clone();
        }

        let decorate = |mut choice: ShotChoice| {
            if let Some(belief) = state.map_belief.as_ref() {
                if let Some(hypothesis) = &belief.field_hypothesis {
                    let shot = ShotObservation {
                        player_id: own_id,
                        angle_degrees: f64::from(choice.angle_degrees),
                        velocity: f64::from(choice.velocity),
                        points: vec![start.clone()],
                    };
                    let path = if choice.used_field_hypothesis {
                        belief.hypothesis_local_field.as_ref().and_then(|field| {
                            simulate_targeting_shot_with_hypothesis(
                                &belief.map,
                                field,
                                hypothesis,
                                &shot,
                                self.world,
                                self.settings.max_simulation_ticks.max(2),
                                &players,
                                self.settings.target_radius,
                            )
                            .ok()
                        })
                    } else {
                        simulate_targeting_shot_with_local_field(
                            &belief.map,
                            &belief.local_field,
                            &shot,
                            self.world,
                            self.settings.max_simulation_ticks.max(2),
                            &players,
                            self.settings.target_radius,
                        )
                        .ok()
                    };
                    choice.hypothesis_uncertainty_proxy = path.and_then(|path| {
                        path.points
                            .iter()
                            .map(|point| {
                                hypothesis.relative_uncertainty_proxy(Vec2::new(
                                    f64::from(point.x),
                                    f64::from(point.y),
                                ))
                            })
                            .reduce(f64::max)
                    });
                }
            }
            choice
        };

        let mut hits = pool
            .iter()
            .filter(|candidate| candidate.predicted_hit)
            .copied()
            .collect::<Vec<_>>();
        hits.sort_by(compare_hit_quality);
        if let Some(mut best_hit) = hits.first().copied() {
            if let Some(belief) = state
                .map_belief
                .as_ref()
                .filter(|belief| belief.field_hypothesis.is_some())
            {
                // A small shortlist is checked against a separate plausible field without the
                // extrapolated hypothesis. Agreement is evidence of robustness, not probability.
                for mut candidate in hits.iter().copied().take(12) {
                    if !running() {
                        return None;
                    }
                    let shot = ShotObservation {
                        player_id: own_id,
                        angle_degrees: f64::from(candidate.angle_degrees),
                        velocity: f64::from(candidate.velocity),
                        points: vec![start.clone()],
                    };
                    let alternative = if candidate.used_field_hypothesis {
                        simulate_targeting_shot_with_local_field(
                            &belief.map,
                            &belief.local_field,
                            &shot,
                            self.world,
                            self.settings.max_simulation_ticks.max(2),
                            &players,
                            self.settings.target_radius,
                        )
                    } else if let (Some(hypothesis), Some(field)) =
                        (&belief.field_hypothesis, &belief.hypothesis_local_field)
                    {
                        simulate_targeting_shot_with_hypothesis(
                            &belief.map,
                            field,
                            hypothesis,
                            &shot,
                            self.world,
                            self.settings.max_simulation_ticks.max(2),
                            &players,
                            self.settings.target_radius,
                        )
                    } else {
                        continue;
                    };
                    candidate.alternative_agrees = alternative.ok().map(|path| matches!(path.termination, Termination::PlayerCollision(id) if Some(id) == candidate.predicted_victim));
                    if candidate.velocity == best_hit.velocity
                        && candidate.angle_degrees == best_hit.angle_degrees
                        && candidate.used_field_hypothesis == best_hit.used_field_hypothesis
                    {
                        best_hit = candidate;
                    }
                    if candidate.alternative_agrees == Some(true) {
                        best_hit = candidate;
                        break;
                    }
                }
            }
            return Some(decorate(ShotChoice {
                reason: ShotReason::Hit,
                ..best_hit
            }));
        }

        for candidate in pool.iter_mut() {
            if !running() {
                return None;
            }
            if !candidate.uncertainty_available || candidate.miss_distance >= 1.0e9 {
                continue;
            }
            candidate.uncertainty_available = false;
            candidate.information_score = state
                .map_belief
                .as_ref()
                .map(|belief| {
                    let shot = ShotObservation {
                        player_id: own_id,
                        angle_degrees: f64::from(candidate.angle_degrees),
                        velocity: f64::from(candidate.velocity),
                        points: vec![start.clone()],
                    };
                    let trajectory = if candidate.used_field_hypothesis {
                        match (&belief.field_hypothesis, &belief.hypothesis_local_field) {
                            (Some(hypothesis), Some(field)) => {
                                simulate_targeting_shot_with_hypothesis(
                                    &belief.map,
                                    field,
                                    hypothesis,
                                    &shot,
                                    self.world,
                                    self.settings.max_simulation_ticks.max(2),
                                    &players,
                                    self.settings.target_radius,
                                )
                            }
                            _ => return 0.0,
                        }
                    } else {
                        simulate_targeting_shot_with_local_field(
                            &belief.map,
                            &belief.local_field,
                            &shot,
                            self.world,
                            self.settings.max_simulation_ticks.max(2),
                            &players,
                            self.settings.target_radius,
                        )
                    };
                    let Ok(trajectory) = trajectory else {
                        return 0.0;
                    };
                    let uncertainty_sum = trajectory.points.iter().try_fold(0.0, |sum, point| {
                        belief
                            .field_uncertainty(Vec2::new(f64::from(point.x), f64::from(point.y)))
                            .map(|uncertainty| sum + uncertainty)
                    });
                    let Some(uncertainty_sum) = uncertainty_sum else {
                        return 0.0;
                    };
                    candidate.uncertainty_available = true;
                    uncertainty_sum / trajectory.points.len().max(1) as f64
                        * if self.settings.coverage_penalty > 0.0 {
                            1.0 - candidate.covered_fraction
                        } else {
                            1.0
                        }
                })
                .unwrap_or(0.0);
        }

        if let Some(best_information) = pool
            .iter()
            .filter(|candidate| {
                candidate.uncertainty_available
                    && !candidate.contact_before_target
                    && candidate.information_score >= self.settings.information_threshold
            })
            .max_by(|left, right| {
                (left.information_score * self.settings.information_weight
                    / f64::from(left.velocity))
                .total_cmp(
                    &(right.information_score * self.settings.information_weight
                        / f64::from(right.velocity)),
                )
            })
        {
            return Some(decorate(ShotChoice {
                reason: ShotReason::Information,
                ..*best_information
            }));
        }

        pool.into_iter()
            .min_by(|left, right| {
                left.contact_before_target
                    .cmp(&right.contact_before_target)
                    .then_with(|| {
                        (left.miss_distance
                            + self.settings.coverage_penalty * left.covered_fraction)
                            .total_cmp(
                                &(right.miss_distance
                                    + self.settings.coverage_penalty * right.covered_fraction),
                            )
                    })
                    .then_with(|| left.velocity.cmp(&right.velocity))
            })
            .map(decorate)
    }
}

/// Distance and fractional segment index at the closest target passage.
fn closest_path_passage(target: &Vec2, points: &[Point], target_radius: f64) -> (f64, f64) {
    let (distance, closest) = points
        .windows(2)
        .enumerate()
        .map(|(index, segment)| {
            let start = Vec2::new(f64::from(segment[0].x), f64::from(segment[0].y));
            let delta = Vec2::new(
                f64::from(segment[1].x) - start.x,
                f64::from(segment[1].y) - start.y,
            );
            let fraction = if delta.norm_squared() == 0.0 {
                0.0
            } else {
                (((target.x - start.x) * delta.x + (target.y - start.y) * delta.y)
                    / delta.norm_squared())
                .clamp(0.0, 1.0)
            };
            let distance = ((target.x - start.x - fraction * delta.x).powi(2)
                + (target.y - start.y - fraction * delta.y).powi(2))
            .sqrt();
            (distance, index as f64 + fraction)
        })
        .min_by(|left, right| left.0.total_cmp(&right.0))
        .unwrap_or((f64::INFINITY, 0.0));
    (
        distance,
        first_circle_entry(target, target_radius, points).unwrap_or(closest),
    )
}

fn first_circle_entry(center: &Vec2, radius: f64, points: &[Point]) -> Option<f64> {
    points.windows(2).enumerate().find_map(|(index, segment)| {
        let x = f64::from(segment[0].x) - center.x;
        let y = f64::from(segment[0].y) - center.y;
        let dx = f64::from(segment[1].x) - f64::from(segment[0].x);
        let dy = f64::from(segment[1].y) - f64::from(segment[0].y);
        let c = x * x + y * y - radius * radius;
        if c <= 0.0 {
            return Some(index as f64);
        }
        let a = dx * dx + dy * dy;
        let b = x * dx + y * dy;
        let discriminant = b * b - a * c;
        if a == 0.0 || discriminant < 0.0 {
            return None;
        }
        let fraction = (-b - discriminant.sqrt()) / a;
        (0.0..=1.0)
            .contains(&fraction)
            .then_some(index as f64 + fraction)
    })
}

fn contact_before_target(planet: &crate::protocol::Planet, points: &[Point], passage: f64) -> bool {
    first_circle_entry(&Vec2::new(planet.x, planet.y), planet.radius, points)
        .is_some_and(|entry| entry < passage)
}

fn compare_hit_quality(left: &ShotChoice, right: &ShotChoice) -> Ordering {
    // Until the model predicts hits reliably, prefer the most central impact. Save
    // energy only when predicted aim quality is equal.
    left.miss_distance
        .total_cmp(&right.miss_distance)
        .then_with(|| left.velocity.cmp(&right.velocity))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_field_state() -> BotState {
        let mut state = BotState::default();
        state.observe(&ServerEvent::OwnId { player_id: 1 });
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 100.0,
            y: 100.0,
        });
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 500.0,
            y: 100.0,
        });
        state
    }

    #[test]
    fn gravity_uncertainty_propagates_parameter_correlations() {
        let map = PlanetMap {
            planets: vec![crate::protocol::Planet {
                x: 0.0,
                y: 0.0,
                radius: 1.0,
                mass: 8.0,
            }],
        };
        let mut parameter_covariance = DMatrix::zeros(3, 3);
        // Perfectly correlated center-x and mass errors reinforce at (2, 0).
        parameter_covariance[(0, 0)] = 1.0;
        parameter_covariance[(2, 2)] = 64.0;
        parameter_covariance[(0, 2)] = 8.0;
        parameter_covariance[(2, 0)] = 8.0;
        let belief = MapBelief::new(map, Some(parameter_covariance));

        let uncertainty = belief
            .field_uncertainty(Vec2::new(2.0, 0.0))
            .expect("matching covariance");
        assert!((uncertainty - 4.0).abs() < 1e-12);
    }

    #[test]
    fn validated_source_disagreement_contributes_broad_gravity_uncertainty() {
        let mut belief = MapBelief::new(PlanetMap { planets: vec![] }, None);
        belief.field_hypothesis = Some(crate::field_hypothesis::FieldHypothesis {
            x: 600.0,
            y: 500.0,
            mass: 2700.0,
            support: vec![],
            support_paths: vec![],
            baseline_continuation_rmse: 3.0,
            continuation_rmse: 0.6,
            training_paths: 3,
            validation_paths: 1,
            relative_improvement: 0.8,
        });
        belief.hypothesis_local_field = Some(crate::local_field::LocalField::default());
        let far_from_support = Vec2::new(200.0, 500.0);
        let early = belief
            .field_uncertainty(far_from_support)
            .expect("source/no-source disagreement is available without covariance");
        belief.field_hypothesis.as_mut().unwrap().validation_paths = 8;
        let corroborated = belief.field_uncertainty(far_from_support).unwrap();
        assert!(early > 0.0);
        assert!(
            corroborated < early,
            "independent support reduces model-choice spread"
        );
    }

    #[test]
    fn diagnostic_snapshot_restores_targeting_history() {
        let mut state = flat_field_state();
        state.observe(&observed_shot((100.0, 100.0), 3.0, 15));
        state.observe(&ServerEvent::ShotBegin {
            player_id: 2,
            angle: -30.0,
            velocity: 4.0,
        });
        let snapshot = state.diagnostic_snapshot();
        let restored = BotState::from_diagnostic_snapshot(&snapshot).unwrap();

        assert_eq!(restored.own_id, state.own_id);
        assert_eq!(restored.player_positions, state.player_positions);
        assert_eq!(restored.known_shots, state.known_shots);
        assert_eq!(restored.covered_cells, state.covered_cells);
        assert_eq!(restored.in_flight, state.in_flight);
    }

    #[test]
    fn hit_quality_precedes_energy_conservation() {
        let grazing_low_energy = ShotChoice {
            velocity: 2,
            miss_distance: 3.5,
            ..test_choice()
        };
        let central_high_energy = ShotChoice {
            velocity: 8,
            miss_distance: 0.5,
            ..test_choice()
        };

        assert_eq!(
            compare_hit_quality(&central_high_energy, &grazing_low_energy),
            Ordering::Less
        );
        assert_eq!(
            compare_hit_quality(
                &grazing_low_energy,
                &ShotChoice {
                    miss_distance: 3.5,
                    velocity: 4,
                    ..test_choice()
                }
            ),
            Ordering::Less,
            "energy remains the tie-breaker when hit quality is equal"
        );
    }

    fn test_choice() -> ShotChoice {
        ShotChoice {
            velocity: 1,
            angle_degrees: 0,
            miss_distance: 0.0,
            information_score: 0.0,
            uncertainty_available: false,
            covered_fraction: 0.0,
            contact_before_target: false,
            reason: ShotReason::Hit,
            predicted_hit: true,
            used_field_hypothesis: false,
            predicted_victim: Some(2),
            alternative_agrees: None,
            hypothesis_uncertainty_proxy: None,
            local_support_fraction: None,
        }
    }

    fn observed_shot(launch: (f32, f32), velocity: f64, angle_degrees: i32) -> ServerEvent {
        ServerEvent::ShotFin {
            player_id: 9,
            angle: f64::from(angle_degrees),
            velocity,
            points: vec![Point {
                x: launch.0,
                y: launch.1,
            }],
        }
    }

    #[test]
    fn an_early_target_entry_precedes_a_later_obstacle_and_closer_passage() {
        let points = vec![
            Point { x: 0.0, y: 3.0 },
            Point { x: 20.0, y: 3.0 },
            Point { x: 30.0, y: 3.0 },
            Point { x: 10.0, y: 0.0 },
        ];
        let target = Vec2::new(10.0, 0.0);
        let planet = crate::protocol::Planet {
            x: 25.0,
            y: 3.0,
            radius: 2.0,
            mass: 1.0,
        };
        let (_, passage) = closest_path_passage(&target, &points, 4.0);
        assert!(
            !contact_before_target(&planet, &points, passage),
            "a later planet cannot undo an earlier hit"
        );
    }

    #[test]
    fn targeting_search_stops_when_cancelled() {
        let state = flat_field_state();
        let mut calls = 0;
        let choice = TargetingPolicy::default().choose_with_check(&state, 50.0, || {
            calls += 1;
            calls < 10
        });
        assert!(choice.is_none());
        assert_eq!(calls, 10);
    }

    #[test]
    fn singular_hypothesis_does_not_hide_a_clear_alternative_hit() {
        let mut state = flat_field_state();
        let mut belief = MapBelief::new(PlanetMap { planets: vec![] }, None);
        belief.field_hypothesis = Some(crate::field_hypothesis::FieldHypothesis {
            x: 100.0,
            y: 100.0,
            mass: 2700.0,
            support: vec![Point { x: 100.0, y: 100.0 }, Point { x: 110.0, y: 100.0 }],
            support_paths: vec![],
            baseline_continuation_rmse: 10.0,
            continuation_rmse: 1.0,
            training_paths: 1,
            validation_paths: 1,
            relative_improvement: 0.9,
        });
        belief.hypothesis_local_field = Some(crate::local_field::LocalField::default());
        state.set_map_belief(Some(belief));
        let choice = TargetingPolicy::default()
            .choose(&state, 8.0)
            .expect("clear alternative shot");
        assert_eq!(choice.reason, ShotReason::Hit);
        assert_eq!(choice.angle_degrees, 0);
        assert_eq!(choice.alternative_agrees, None);
    }

    #[test]
    fn policy_refines_angles_between_coarse_samples() {
        let mut state = flat_field_state();
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 300.0,
            y: 89.5,
        });
        let choice = TargetingPolicy::default().choose(&state, 2.0).unwrap();
        assert_eq!(choice.reason, ShotReason::Hit);
        assert!((2..=4).contains(&choice.angle_degrees));
    }

    #[test]
    fn policy_uses_available_energy_to_reach_a_distant_player() {
        let mut state = flat_field_state();
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 1700.0,
            y: 100.0,
        });
        let choice = TargetingPolicy::default().choose(&state, 9.0).unwrap();
        assert_eq!(choice.reason, ShotReason::Hit);
        assert_eq!(choice.velocity, 9);
    }

    #[test]
    fn policy_avoids_repeating_a_known_shot_from_the_same_launch_point() {
        let mut state = flat_field_state();
        let policy = TargetingPolicy::default();

        // Flat field, straight line at the target: the first velocity whose capped path
        // reaches 400 units away is 3 (velocity 2 covers only 180 ticks * 2 units).
        let baseline = policy.choose(&state, 8.0).expect("baseline choice");
        assert_eq!(baseline.reason, ShotReason::Hit);
        assert_eq!(baseline.angle_degrees, 0);
        assert_eq!(baseline.velocity, 3);

        state.observe(&observed_shot(
            (100.0, 100.0),
            f64::from(baseline.velocity),
            baseline.angle_degrees,
        ));
        let next = policy.choose(&state, 8.0).expect("next choice");
        assert_eq!(next.reason, ShotReason::Hit);
        assert_eq!(next.angle_degrees, 0);
        assert!(next.velocity > baseline.velocity);

        // The same inputs observed from a different launch point do not constrain this one.
        let mut other_start_state = flat_field_state();
        other_start_state.observe(&observed_shot((900.0, 900.0), 3.0, 0));
        let unaffected = policy
            .choose(&other_start_state, 8.0)
            .expect("unaffected choice");
        assert_eq!(
            (unaffected.velocity, unaffected.angle_degrees),
            (baseline.velocity, baseline.angle_degrees)
        );
    }

    #[test]
    fn policy_avoids_repeating_a_shot_still_in_flight() {
        let mut state = flat_field_state();
        let policy = TargetingPolicy::default();
        let baseline = policy.choose(&state, 8.0).expect("baseline choice");
        assert_eq!(baseline.reason, ShotReason::Hit);
        assert_eq!(baseline.velocity, 3);

        // The bot fired and the shot is in the air; before it lands, the next energy
        // response must produce a different choice.
        state.observe(&ServerEvent::ShotBegin {
            player_id: 1,
            angle: f64::from(baseline.angle_degrees),
            velocity: f64::from(baseline.velocity),
        });
        let next = policy.choose(&state, 8.0).expect("in-flight-aware choice");
        assert_ne!(
            (next.velocity, next.angle_degrees),
            (baseline.velocity, baseline.angle_degrees)
        );

        // Another player's in-flight shot from elsewhere does not constrain this bot.
        let mut other_state = flat_field_state();
        other_state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 500.0,
            y: 100.0,
        });
        other_state.observe(&ServerEvent::ShotBegin {
            player_id: 2,
            angle: f64::from(baseline.angle_degrees),
            velocity: f64::from(baseline.velocity),
        });
        let unaffected = policy.choose(&other_state, 8.0).expect("unaffected choice");
        assert_eq!(
            (unaffected.velocity, unaffected.angle_degrees),
            (baseline.velocity, baseline.angle_degrees)
        );

        // When the in-flight shot finishes it stays excluded as a completed shot.
        state.observe(&ServerEvent::ShotFin {
            player_id: 1,
            angle: f64::from(baseline.angle_degrees),
            velocity: f64::from(baseline.velocity),
            points: vec![Point { x: 100.0, y: 100.0 }, Point { x: 200.0, y: 100.0 }],
        });
        let after = policy.choose(&state, 8.0).expect("completed-aware choice");
        assert_ne!(
            (after.velocity, after.angle_degrees),
            (baseline.velocity, baseline.angle_degrees)
        );
    }

    #[test]
    fn map_change_forgets_completed_and_in_flight_shots() {
        // Old-map paths must not exclude shots when a new map reuses a launch position.
        let mut state = flat_field_state();
        let launch = Point { x: 100.0, y: 100.0 };
        state.observe(&observed_shot((100.0, 100.0), 3.0, 0));
        state.observe(&ServerEvent::ShotBegin {
            player_id: 1,
            angle: 10.0,
            velocity: 4.0,
        });
        assert!(state.is_known_shot(&launch, 3, 0));
        assert!(state.is_known_shot(&launch, 4, 10));

        state.observe(&ServerEvent::Planets { planets: vec![] });

        assert!(!state.is_known_shot(&launch, 3, 0));
        assert!(!state.is_known_shot(&launch, 4, 10));
    }

    #[test]
    fn policy_falls_back_to_known_shots_when_every_candidate_is_known() {
        let mut state = flat_field_state();
        for angle in -180..=180 {
            state.observe(&observed_shot((100.0, 100.0), 2.0, angle));
        }
        let policy = TargetingPolicy::default();

        // Energy 2 allows only velocity 2; every angle is known, so the unfiltered best
        // (angle 0, closest approach) is still fired rather than skipping the shot.
        let choice = policy.choose(&state, 2.0).expect("fallback choice");
        assert_eq!((choice.velocity, choice.angle_degrees), (2, 0));
        assert_eq!(choice.reason, ShotReason::Approach);
    }
}
