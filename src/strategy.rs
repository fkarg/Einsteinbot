use crate::coverage::path_cells;
use crate::estimator::{ResidualCorrection, ResidualGate, ResidualModel};
use crate::in_flight::{
    DEFAULT_STEP_RATE, FIRE_LATENCY_MS, InFlightShot, SHOT_SLOTS, doom_probability, predict_kills,
};
use crate::map::{PlanetMap, Vec2};
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_corrected_targeting_shot,
};
use nalgebra::DMatrix;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// One retained alternative map with its approximate-evidence weight.
#[derive(Clone, Debug)]
pub struct WeightedMap {
    pub map: PlanetMap,
    /// Normalized approximate-evidence weight among the retained modes. This is a heuristic
    /// ranking, not a calibrated probability.
    pub weight: f64,
}

/// What the estimator publishes. `map` is the maximum-weight mode; targeting simulates only it.
#[derive(Clone, Debug)]
pub struct MapBelief {
    pub map: PlanetMap,
    /// Approximate-evidence weight of `map` among the retained modes (not a calibrated
    /// probability). `None` when no evidence weighting was computed or when the search was cut short
    /// (`search_limit` set): the weights are normalized over the modes found, so they would read
    /// as certainty.
    pub map_weight: Option<f64>,
    /// Other retained modes in descending weight, at most a few. Not consulted by targeting yet.
    pub alternatives: Vec<WeightedMap>,
    /// Covariance of `map` ordered by `[center_x, center_y, mass]` for each planet.
    pub parameter_covariance: Option<DMatrix<f64>>,
    /// Why `parameter_covariance` is unavailable. Unavailable is never zero uncertainty.
    pub covariance_unavailable_reason: Option<crate::estimator::CovarianceUnavailableReason>,
    /// Terminal points classified as possible planet surface contacts.
    pub contact_points: Vec<Point>,
    /// Possible contacts farther than the full-strength contact range (15 units) from the surface
    /// of every planet in `map` (a model mismatch, or planets this slice does not model). With an
    /// empty map this is all of them.
    pub unmatched_contacts: usize,
    /// Coordinate RMSE of `map` against the regular samples of every path fitted. The live
    /// estimator no longer computes it (P18: it re-simulated every path on each publication and
    /// is misleading under chaotic divergence); `None` is unavailable. Offline tools compute it
    /// with `estimator::position_rmse`.
    pub model_position_rmse: Option<f64>,
    /// Set when the planet-count search stopped at its cap or time budget rather than by running
    /// out of proposals.
    pub search_limit: Option<crate::estimator::SearchLimit>,
    /// Residual correction for aiming (`estimator::residual`): the smoothed model error along
    /// the observed paths, spread to nearby space. Targeting adds it to the planets' field.
    pub residual_correction: Option<ResidualCorrection>,
    /// Why `residual_correction` is absent (gated off or not computed). Absent is not zero
    /// model error.
    pub residual_correction_unavailable_reason: Option<ResidualGate>,
    /// Model-error scalars the correction was (or would have been) built with; with `map` and
    /// the epoch's paths they rebuild it (`estimator::residual_correction`).
    pub residual_model: Option<ResidualModel>,
    /// Whether the planet cap is likely below the true count (P23); `None`: not assessed (the
    /// belief did not come from the estimator).
    pub count_limit: Option<crate::estimator::CountLimit>,
}

impl MapBelief {
    pub fn new(map: PlanetMap, parameter_covariance: Option<DMatrix<f64>>) -> Self {
        let covariance_unavailable_reason = parameter_covariance
            .is_none()
            .then_some(crate::estimator::CovarianceUnavailableReason::NotComputed);
        Self {
            map,
            map_weight: None,
            alternatives: Vec::new(),
            parameter_covariance,
            covariance_unavailable_reason,
            contact_points: Vec::new(),
            unmatched_contacts: 0,
            model_position_rmse: None,
            search_limit: None,
            residual_correction: None,
            residual_correction_unavailable_reason: Some(ResidualGate::NotComputed),
            residual_model: None,
            count_limit: None,
        }
    }

    /// The residual correction at `position` (acceleration units); `None` when unavailable
    /// (`residual_correction_unavailable_reason`), never a silent zero.
    pub fn residual_correction_at(&self, position: Vec2) -> Option<Vec2> {
        self.residual_correction
            .as_ref()
            .map(|correction| correction.at(position))
    }

    /// The belief as recorded in capture diagnostics (`map_belief`). Dropped fields are simply
    /// absent; consumers must treat absent as unavailable, never zero.
    pub fn diagnostic_json(&self) -> serde_json::Value {
        serde_json::json!({
            "planets": self.map.planets,
            "contact_points": self.contact_points,
            "covariance": self.parameter_covariance.as_ref().map(|matrix| {
                (0..matrix.nrows())
                    .map(|row| (0..matrix.ncols()).map(|col| matrix[(row, col)]).collect::<Vec<_>>())
                    .collect::<Vec<_>>()
            }),
            "covariance_unavailable_reason": self.covariance_unavailable_reason,
            "map_weight": self.map_weight,
            "alternatives": self.alternatives.iter().map(|alternative| serde_json::json!({
                "planets": alternative.map.planets,
                "weight": alternative.weight,
            })).collect::<Vec<_>>(),
            "unmatched_contacts": self.unmatched_contacts,
            "model_position_rmse": self.model_position_rmse,
            "search_limit": self.search_limit,
            "residual_correction": self.residual_correction.as_ref().map(|c| serde_json::json!({
                "length": c.length,
                "max_magnitude": c.max_magnitude,
                "rows": c.rows,
            })),
            "residual_correction_unavailable_reason": self.residual_correction_unavailable_reason,
            "residual_model": self.residual_model,
            "count_limit": self.count_limit,
        })
    }

    fn field_uncertainty(&self, position: Vec2) -> Option<f64> {
        let parameters = self.parameter_covariance.as_ref()?;
        self.map
            .acceleration_covariance(position, parameters)
            .ok()
            .map(|covariance| (covariance.xx + covariance.yy).max(0.0).sqrt())
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
struct KnownShot {
    launch: Point,
    velocity: f64,
    angle_degrees: f64,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct BotState {
    own_id: Option<u32>,
    player_positions: BTreeMap<u32, Point>,
    /// Distinct spawn-like positions reported in this map epoch. These are observed
    /// possibilities, not a complete catalogue; a later spawn may be somewhere new.
    #[serde(default)]
    observed_spawn_locations: Vec<Point>,
    #[serde(skip)]
    map_belief: Option<MapBelief>,
    known_shots: Vec<KnownShot>,
    covered_cells: BTreeSet<(i32, i32)>,
    in_flight: Vec<InFlightShot>,
    /// Local time (unix ms) of the latest observed event, or set for a decision; `None` when
    /// events are observed without a clock (doomed-target avoidance is then off).
    #[serde(default)]
    clock_ms: Option<i64>,
    /// Server steps per second measured from begin/finish pairs (`None`: not measured yet).
    #[serde(default)]
    step_rate: Option<f64>,
    /// Shots begun per player (slot reuse, `in_flight::SHOT_SLOTS`).
    #[serde(default)]
    begin_counts: BTreeMap<u32, u64>,
}

impl BotState {
    /// Restore the bot's captured targeting state for offline inspection and replay.
    pub fn from_diagnostic_snapshot(
        snapshot: &serde_json::Value,
    ) -> Result<Self, serde_json::Error> {
        serde_json::from_value(snapshot.clone())
    }

    /// Forget everything tied to the previous map (shots, coverage, belief, spawn sightings).
    /// Called at every detected map boundary; a `Planets` message calls it too.
    pub fn start_new_map(&mut self) {
        self.known_shots.clear();
        self.covered_cells.clear();
        self.map_belief = None;
        self.in_flight.clear();
        // During map reinitialization, NewtonWars sends each player's new position
        // immediately before that player's Planets message. Other entries may still
        // be stale because those players' updates can arrive after this boundary.
        self.observed_spawn_locations.clear();
        if let Some(own_id) = self.own_id
            && let Some(position) = self.player_positions.get(&own_id)
        {
            self.observed_spawn_locations.push(position.clone());
        }
    }

    /// Observe an event without a clock (tests, offline tools): in-flight shots get no begin time.
    pub fn observe(&mut self, event: &ServerEvent) {
        self.observe_event(event, None);
    }

    /// Observe an event received at `now_ms` (local unix ms).
    pub fn observe_at(&mut self, event: &ServerEvent, now_ms: i64) {
        self.clock_ms = Some(now_ms);
        self.observe_event(event, Some(now_ms));
    }

    fn observe_event(&mut self, event: &ServerEvent, now_ms: Option<i64>) {
        match event {
            ServerEvent::Planets { .. } => self.start_new_map(),
            ServerEvent::OwnId { player_id } => self.own_id = Some(*player_id),
            ServerEvent::PlayerPosition { player_id, x, y } => {
                let position = Point { x: *x, y: *y };
                if !self.observed_spawn_locations.contains(&position) {
                    self.observed_spawn_locations.push(position.clone());
                }
                self.set_player_position(*player_id, position);
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
                let ordinal = self.begin_counts.entry(*player_id).or_insert(0);
                *ordinal += 1;
                let ordinal = *ordinal;
                // The new shot takes the slot of the shooter's 16th-last one, ending it silently.
                self.in_flight.retain(|shot| {
                    shot.player_id != *player_id || shot.ordinal + SHOT_SLOTS > ordinal
                });
                self.in_flight.push(InFlightShot {
                    player_id: *player_id,
                    launch: self.player_positions.get(player_id).cloned(),
                    velocity: *velocity,
                    angle_degrees: *angle,
                    begun_ms: now_ms,
                    ordinal,
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
                    let shot = self.in_flight.swap_remove(index);
                    if let (Some(begun_ms), Some(now_ms)) = (shot.begun_ms, now_ms) {
                        self.measure_step_rate(points.len(), now_ms - begun_ms);
                    }
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

    /// Fold one finished shot's duration into the step-rate estimate. Short flights are skipped:
    /// receive jitter of a step or two dominates them.
    fn measure_step_rate(&mut self, point_count: usize, duration_ms: i64) {
        if duration_ms < 1_000 || point_count < 2 {
            return;
        }
        let sample = (point_count - 1) as f64 / (duration_ms as f64 / 1000.0);
        self.step_rate = Some(match self.step_rate {
            Some(rate) => 0.9 * rate + 0.1 * sample,
            None => sample,
        });
    }

    /// Server steps per second: measured, else `in_flight::DEFAULT_STEP_RATE`.
    pub fn step_rate(&self) -> f64 {
        self.step_rate.unwrap_or(DEFAULT_STEP_RATE)
    }

    /// Set the decision time (local unix ms) the policy times in-flight shots against.
    pub fn set_clock_ms(&mut self, now_ms: i64) {
        self.clock_ms = Some(now_ms);
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
            "observed_spawn_locations": self.observed_spawn_locations,
            "spawn_locations_complete": false,
            "new_spawn_locations_possible": true,
            "known_shots": self.known_shots,
            "in_flight": self.in_flight,
            "covered_cells": self.covered_cells,
            "clock_ms": self.clock_ms,
            "step_rate": self.step_rate,
            "begin_counts": self.begin_counts,
        })
    }

    pub fn player_positions(&self) -> &BTreeMap<u32, Point> {
        &self.player_positions
    }

    /// Update a position from a simulated/test state that is not evidence of a spawn.
    pub fn set_player_position(&mut self, player_id: u32, position: Point) {
        self.player_positions.insert(player_id, position);
    }

    /// Distinct positions reported during the current map epoch. More locations may exist.
    pub fn observed_spawn_locations(&self) -> &[Point] {
        &self.observed_spawn_locations
    }

    pub fn own_id(&self) -> Option<u32> {
        self.own_id
    }

    pub fn set_map_belief(&mut self, map_belief: Option<MapBelief>) {
        self.map_belief = map_belief;
    }
}

/// Below this energy a shot without a predicted hit is not fired; the bot waits for energy.
pub const APPROACH_MIN_ENERGY: f64 = 15.0;
/// Hit ranking weight of one unit of predicted impact offset, in flight ticks.
const MISS_DISTANCE_TICKS: f64 = 5.0;

/// Loadable from JSON (`devtool eval --config`): missing keys take the defaults, unknown keys are
/// errors.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetingSettings {
    pub angle_step_degrees: u16,
    pub max_simulation_ticks: usize,
    pub target_radius: f64,
    /// Average acceleration standard deviation required before probing is considered.
    pub information_threshold: f64,
    pub information_weight: f64,
    /// Maximum approach-distance penalty for retracing observed space (world units).
    pub coverage_penalty: f64,
    /// A predicted hit whose victim an in-flight shot kills first with at least this chance is
    /// skipped (`in_flight::doom_probability`); above 1 disables doomed-target avoidance.
    pub doom_threshold: f64,
    /// Hit ranking: flight ticks plus this many ticks per unit of predicted impact offset.
    pub miss_distance_ticks: f64,
    /// Below this energy a shot without a predicted hit is not fired.
    pub approach_min_energy: f64,
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
            doom_threshold: 0.15,
            miss_distance_ticks: MISS_DISTANCE_TICKS,
            approach_min_energy: APPROACH_MIN_ENERGY,
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
    pub predicted_victim: Option<u32>,
    /// Simulated ticks until the path ends (impact for a predicted hit).
    pub flight_ticks: usize,
    /// Chance the predicted victim is killed by an in-flight shot before this one lands;
    /// `None` when not computed (no predicted hit, or no clock).
    pub victim_doom: Option<f64>,
    /// Predicted hits skipped because their victim was doomed.
    pub doomed_hits_skipped: usize,
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
        let correction = state
            .map_belief
            .as_ref()
            .and_then(|belief| belief.residual_correction.as_ref());
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
        let evaluate = |velocity: i32, angle: i32| -> Option<ShotChoice> {
            let shot = ShotObservation {
                player_id: own_id,
                angle_degrees: f64::from(angle),
                velocity: f64::from(velocity),
                points: vec![start.clone()],
            };
            let result = simulate_corrected_targeting_shot(
                map,
                correction,
                &shot,
                self.world,
                self.settings.max_simulation_ticks.max(2),
                &players,
                self.settings.target_radius,
            );
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
                predicted_victim: match trajectory.termination {
                    Termination::PlayerCollision(id) if id != own_id => Some(id),
                    _ => None,
                },
                flight_ticks: trajectory.points.len(),
                victim_doom: None,
                doomed_hits_skipped: 0,
            })
        };
        let mut candidates = Vec::new();
        for velocity in 2..=max_velocity {
            let mut angles = (-180..=180).step_by(angle_step).collect::<Vec<_>>();
            if angles.last() != Some(&180) {
                angles.push(180);
            }
            for angle in angles {
                if !running() {
                    return None;
                }
                if let Some(candidate) = evaluate(velocity, angle) {
                    candidates.push(candidate);
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
            .map(|c| (c.velocity, c.angle_degrees))
            .collect::<BTreeSet<_>>();
        let mut seed_counts = BTreeMap::new();
        let mut refined = 0;
        for seed in seeds.into_iter().filter(|seed| {
            let count = seed_counts.entry(seed.angle_degrees).or_insert(0);
            *count += 1;
            if *count > 2 {
                return false;
            }
            refined += 1;
            refined <= 12
        }) {
            for angle in (seed.angle_degrees - angle_step as i32 + 1)
                ..(seed.angle_degrees + angle_step as i32)
            {
                if !running() {
                    return None;
                }
                let angle = (angle + 180).rem_euclid(360) - 180;
                if sampled.insert((seed.velocity, angle)) {
                    if let Some(candidate) = evaluate(seed.velocity, angle) {
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

        let mut hits = pool
            .iter()
            .filter(|candidate| candidate.predicted_hit)
            .copied()
            .collect::<Vec<_>>();
        hits.sort_by(|a, b| compare_hit_quality(a, b, self.settings.miss_distance_ticks));
        // Doomed-target avoidance: a hit on a player another in-flight shot (ours included)
        // kills first is wasted, so skip such hits unless ours lands first (the race rule).
        if let (Some(now_ms), false) = (state.clock_ms, hits.is_empty()) {
            let kills = predict_kills(
                map,
                correction,
                &state.in_flight,
                &players,
                now_ms,
                state.step_rate(),
                self.world,
                self.settings.max_simulation_ticks,
                self.settings.target_radius,
            );
            let ms_per_tick = 1000.0 / state.step_rate();
            for hit in &mut hits {
                let our_impact_ms =
                    now_ms as f64 + FIRE_LATENCY_MS + hit.flight_ticks as f64 * ms_per_tick;
                hit.victim_doom = Some(doom_probability(
                    &kills,
                    hit.predicted_victim.unwrap(),
                    our_impact_ms,
                ));
            }
            let skipped = hits
                .iter()
                .filter(|hit| hit.victim_doom.unwrap() >= self.settings.doom_threshold)
                .count();
            if skipped == hits.len() {
                // Every predicted hit is doomed: wait for the targets to respawn rather than
                // spend the energy on a shot that lands on nobody.
                return None;
            }
            hits.retain(|hit| hit.victim_doom.unwrap() < self.settings.doom_threshold);
            for hit in &mut hits {
                hit.doomed_hits_skipped = skipped;
            }
        }
        if let Some(best_hit) = hits.first().copied() {
            return Some(ShotChoice {
                reason: ShotReason::Hit,
                ..best_hit
            });
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
                    let trajectory = simulate_corrected_targeting_shot(
                        &belief.map,
                        correction,
                        &shot,
                        self.world,
                        self.settings.max_simulation_ticks.max(2),
                        &players,
                        self.settings.target_radius,
                    );
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
            return Some(ShotChoice {
                reason: ShotReason::Information,
                ..*best_information
            });
        }

        // A cheap shot without a predicted hit rarely hits (capture 55: 5 of 58 at a median
        // energy of 3.6); saving energy until a hit is predicted pays off more.
        if energy < self.settings.approach_min_energy {
            return None;
        }
        pool.into_iter().min_by(|left, right| {
            left.contact_before_target
                .cmp(&right.contact_before_target)
                .then_with(|| {
                    (left.miss_distance + self.settings.coverage_penalty * left.covered_fraction)
                        .total_cmp(
                            &(right.miss_distance
                                + self.settings.coverage_penalty * right.covered_fraction),
                        )
                })
                .then_with(|| left.velocity.cmp(&right.velocity))
        })
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

fn compare_hit_quality(
    left: &ShotChoice,
    right: &ShotChoice,
    miss_distance_ticks: f64,
) -> Ordering {
    // Map error grows along the path, so shorter flights hit far more often than central but
    // long ones (capture 55: 5 of 7 under 30 ticks, 1 of 32 over 100). Rank by flight time,
    // with the impact offset as a small penalty; save energy only on equal scores.
    let score = |choice: &ShotChoice| {
        choice.flight_ticks as f64 + miss_distance_ticks * choice.miss_distance
    };
    score(left)
        .total_cmp(&score(right))
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
        assert_eq!(
            restored.observed_spawn_locations(),
            state.observed_spawn_locations()
        );
    }

    #[test]
    fn spawn_positions_accumulate_as_a_deduplicated_incomplete_map_set() {
        let mut state = BotState::default();
        for (player_id, x, y) in [
            (1, 100.0, 200.0),
            (2, 100.0, 200.0),
            (1, 350.0, 420.0),
            (3, 620.0, 140.0),
        ] {
            state.observe(&ServerEvent::PlayerPosition { player_id, x, y });
        }

        assert_eq!(
            state.observed_spawn_locations(),
            vec![
                Point { x: 100.0, y: 200.0 },
                Point { x: 350.0, y: 420.0 },
                Point { x: 620.0, y: 140.0 },
            ],
            "reused sites deduplicate while newly reported sites extend the map-scoped set"
        );
        let diagnostic = state.diagnostic_snapshot();
        assert_eq!(diagnostic["spawn_locations_complete"], false);
        assert_eq!(diagnostic["new_spawn_locations_possible"], true);
        assert_eq!(
            diagnostic["observed_spawn_locations"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn synthetic_player_positions_do_not_become_spawn_evidence() {
        let mut state = BotState::default();
        state.set_player_position(2, Point { x: 500.0, y: 300.0 });

        assert_eq!(
            state.player_positions().get(&2),
            Some(&Point { x: 500.0, y: 300.0 })
        );
        assert!(state.observed_spawn_locations().is_empty());
    }

    #[test]
    fn map_epoch_keeps_own_fresh_position_and_discards_other_stale_positions() {
        let mut state = BotState::default();
        state.observe(&ServerEvent::OwnId { player_id: 1 });
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 100.0,
            y: 100.0,
        });
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 300.0,
            y: 300.0,
        });
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 500.0,
            y: 500.0,
        });
        // During reinitialize, this player's fresh position is sent immediately before
        // its Planets event, while other players' stored positions may still be stale.
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 300.0,
            y: 300.0,
        });

        state.observe(&ServerEvent::Planets { planets: vec![] });

        assert_eq!(
            state.observed_spawn_locations(),
            vec![Point { x: 300.0, y: 300.0 }],
            "retain the own position just refreshed for this boundary, not other stale map positions"
        );
    }

    #[test]
    fn legacy_diagnostic_snapshot_has_no_spawn_locations_but_allows_new_ones() {
        let snapshot = serde_json::json!({
            "own_id": 1,
            "player_positions": {},
            "known_shots": [],
            "covered_cells": [],
            "in_flight": []
        });
        let restored = BotState::from_diagnostic_snapshot(&snapshot).unwrap();

        assert!(restored.observed_spawn_locations().is_empty());
        assert_eq!(
            restored.diagnostic_snapshot()["spawn_locations_complete"],
            false
        );
    }

    #[test]
    fn hit_ranking_prefers_short_flights_over_central_long_ones() {
        let central_long = ShotChoice {
            flight_ticks: 120,
            miss_distance: 0.0,
            velocity: 3,
            ..test_choice()
        };
        let grazing_short = ShotChoice {
            flight_ticks: 40,
            miss_distance: 3.5,
            velocity: 9,
            ..test_choice()
        };
        assert_eq!(
            compare_hit_quality(&grazing_short, &central_long, MISS_DISTANCE_TICKS),
            Ordering::Less
        );

        // At equal flight time the more central impact wins, then the cheaper shot.
        let central_short = ShotChoice {
            miss_distance: 0.5,
            ..grazing_short
        };
        assert_eq!(
            compare_hit_quality(&central_short, &grazing_short, MISS_DISTANCE_TICKS),
            Ordering::Less
        );
        assert_eq!(
            compare_hit_quality(
                &ShotChoice {
                    velocity: 4,
                    ..central_short
                },
                &central_short,
                MISS_DISTANCE_TICKS
            ),
            Ordering::Less
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
            predicted_victim: Some(2),
            flight_ticks: 30,
            victim_doom: None,
            doomed_hits_skipped: 0,
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

        // Flat field, straight line at the target: the fastest legal shot lands soonest.
        let baseline = policy.choose(&state, 8.0).expect("baseline choice");
        assert_eq!(baseline.reason, ShotReason::Hit);
        assert_eq!(baseline.angle_degrees, 0);
        assert_eq!(baseline.velocity, 8);

        state.observe(&observed_shot(
            (100.0, 100.0),
            f64::from(baseline.velocity),
            baseline.angle_degrees,
        ));
        let next = policy.choose(&state, 8.0).expect("next choice");
        assert_eq!(next.reason, ShotReason::Hit);
        assert_eq!(next.angle_degrees, 0);
        assert!(next.velocity < baseline.velocity);

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
        assert_eq!(baseline.velocity, 8);

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
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 2,
            x: 300.0,
            y: 100.0,
        });
        for angle in -180..=180 {
            state.observe(&observed_shot((100.0, 100.0), 2.0, angle));
        }
        let policy = TargetingPolicy::default();

        // Energy 2 allows only velocity 2; every angle is known, so the unfiltered best
        // (the straight hit) is still fired rather than skipping the shot.
        let choice = policy.choose(&state, 2.0).expect("fallback choice");
        assert_eq!((choice.velocity, choice.angle_degrees), (2, 0));
        assert_eq!(choice.reason, ShotReason::Hit);
    }

    #[test]
    fn policy_waits_for_energy_when_no_hit_is_predicted() {
        // Two ticks reach no target, so no candidate is a predicted hit.
        let policy = TargetingPolicy::new(TargetingSettings {
            max_simulation_ticks: 2,
            ..TargetingSettings::default()
        });
        let state = flat_field_state();

        assert_eq!(policy.choose(&state, APPROACH_MIN_ENERGY - 0.1), None);
        let choice = policy
            .choose(&state, APPROACH_MIN_ENERGY)
            .expect("enough energy for an approach shot");
        assert_eq!(choice.reason, ShotReason::Approach);
    }

    fn begin(state: &mut BotState, player_id: u32, angle: f64, velocity: f64, now_ms: i64) {
        state.observe_at(
            &ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            },
            now_ms,
        );
    }

    #[test]
    fn in_flight_shots_carry_begin_times_and_free_reused_slots() {
        let mut state = flat_field_state();
        for k in 0..SHOT_SLOTS as i64 {
            begin(&mut state, 2, k as f64, 5.0, 1_000 + k);
        }
        assert_eq!(state.in_flight.len(), SHOT_SLOTS as usize);
        assert_eq!(state.in_flight[0].begun_ms, Some(1_000));
        assert_eq!(
            state.in_flight[0].launch,
            Some(Point { x: 500.0, y: 100.0 })
        );
        // The 17th shot reuses the slot of the first, which the server drops without ShotFin.
        begin(&mut state, 2, 99.0, 5.0, 2_000);
        assert_eq!(state.in_flight.len(), SHOT_SLOTS as usize);
        assert!(state.in_flight.iter().all(|shot| shot.angle_degrees != 0.0));
        // Another player's shots keep their own slots.
        begin(&mut state, 1, 0.0, 5.0, 2_000);
        assert_eq!(state.in_flight.len(), SHOT_SLOTS as usize + 1);

        // A finished shot leaves the list and measures the server step rate.
        assert_eq!(state.step_rate(), DEFAULT_STEP_RATE);
        let points = vec![Point { x: 100.0, y: 100.0 }; 61];
        state.observe_at(
            &ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.0,
                velocity: 5.0,
                points,
            },
            4_000,
        );
        assert_eq!(state.in_flight.len(), SHOT_SLOTS as usize);
        assert!((state.step_rate() - 30.0).abs() < 1e-9);
    }

    /// Own player 1 at (100, 100), opponents 2 at (500, 100) and 3 at (100, 400), flat field.
    fn two_target_state() -> BotState {
        let mut state = flat_field_state();
        state.observe_at(
            &ServerEvent::PlayerPosition {
                player_id: 3,
                x: 100.0,
                y: 400.0,
            },
            0,
        );
        state
    }

    #[test]
    fn policy_does_not_double_target_a_player_its_in_flight_shot_kills_first() {
        let policy = TargetingPolicy::default();
        let mut state = two_target_state();
        let baseline = policy.choose(&state, 8.0).expect("baseline");
        assert_eq!(baseline.predicted_victim, Some(3), "player 3 is nearer");

        // Our earlier fast shot is on its way to player 3 and lands before any new shot.
        begin(&mut state, 1, -90.0, 20.0, 0);
        state.set_clock_ms(100);
        let next = policy.choose(&state, 8.0).expect("another target");
        assert_eq!(next.reason, ShotReason::Hit);
        assert_eq!(next.predicted_victim, Some(2));
        assert_eq!(next.victim_doom, Some(0.0));
        assert!(next.doomed_hits_skipped > 0);

        // Avoidance off: the doomed nearer target is chosen again.
        let off = TargetingPolicy::new(TargetingSettings {
            doom_threshold: f64::INFINITY,
            ..TargetingSettings::default()
        });
        assert_eq!(off.choose(&state, 8.0).unwrap().predicted_victim, Some(3));
    }

    #[test]
    fn policy_races_another_players_slow_shot_and_waits_when_every_target_is_doomed() {
        let policy = TargetingPolicy::default();
        let mut state = flat_field_state();
        state.observe_at(
            &ServerEvent::PlayerPosition {
                player_id: 9,
                x: 500.0,
                y: 700.0,
            },
            0,
        );
        // Player 9 shoots straight up at player 2, 4 units per tick: impact after 149 ticks
        // (about 4.9 s at 30.5 steps per second).
        begin(&mut state, 9, 90.0, 4.0, 0);
        state.set_clock_ms(100);
        // Our 8-unit shot reaches player 2 in about 1.7 s: we win the race.
        let race = policy.choose(&state, 8.0).expect("we land first");
        assert_eq!(race.predicted_victim, Some(2));
        assert_eq!(race.victim_doom, Some(0.0));

        // At 4 s the slow shot lands before ours would: player 2 is doomed, aim at player 9.
        state.set_clock_ms(4_000);
        let late = policy.choose(&state, 8.0).expect("the other target");
        assert_eq!(late.predicted_victim, Some(9));

        // Our own fast shot is already on its way to player 9: every target is doomed, wait.
        let angle = -(600.0f64).atan2(400.0).to_degrees();
        begin(&mut state, 1, angle, 30.0, 3_900);
        state.set_clock_ms(4_000);
        assert_eq!(policy.choose(&state, 8.0), None);
    }
}
