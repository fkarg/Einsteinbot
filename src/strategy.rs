use crate::map::{PlanetMap, Vec2};
use crate::protocol::{Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters, simulate_shot};
use nalgebra::DMatrix;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct MapBelief {
    pub map: PlanetMap,
    /// Covariance ordered by `[center_x, center_y, mass]` for each planet.
    pub parameter_covariance: Option<DMatrix<f64>>,
}

impl MapBelief {
    pub fn new(map: PlanetMap, parameter_covariance: Option<DMatrix<f64>>) -> Self {
        Self {
            map,
            parameter_covariance,
        }
    }

    fn field_uncertainty(&self, position: Vec2) -> f64 {
        let Some(covariance) = &self.parameter_covariance else {
            return 0.0;
        };
        let Ok(jacobian) = self.map.acceleration_parameter_jacobian(position) else {
            return 0.0;
        };
        if covariance.nrows() != jacobian.ncols() || covariance.ncols() != jacobian.ncols() {
            return 0.0;
        }
        // Use only marginal parameter variances for this ranking proxy. Full
        // covariance propagation is quadratic in planet count and this path
        // evaluates many candidate trajectories.
        let variance = (0..jacobian.ncols())
            .map(|parameter| {
                (jacobian[(0, parameter)].powi(2) + jacobian[(1, parameter)].powi(2))
                    * covariance[(parameter, parameter)].max(0.0)
            })
            .sum::<f64>();
        variance.max(0.0).sqrt()
    }
}

#[derive(Clone, Debug, PartialEq)]
struct KnownShot {
    launch: Point,
    velocity: f64,
    angle_degrees: f64,
}

/// A shot that began but has not finished yet. `launch` is the player's tracked position when
/// the begin event arrived (`None` when unknown); it never constrains candidates from elsewhere.
#[derive(Clone, Debug, PartialEq)]
struct InFlightShot {
    player_id: u32,
    launch: Option<Point>,
    velocity: f64,
    angle_degrees: f64,
}

#[derive(Clone, Debug, Default)]
pub struct BotState {
    own_id: Option<u32>,
    player_positions: BTreeMap<u32, Point>,
    map_belief: Option<MapBelief>,
    known_shots: Vec<KnownShot>,
    in_flight: Vec<InFlightShot>,
}

impl BotState {
    pub fn observe(&mut self, event: &ServerEvent) {
        match event {
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

    pub fn player_positions(&self) -> &BTreeMap<u32, Point> {
        &self.player_positions
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
}

impl Default for TargetingSettings {
    fn default() -> Self {
        Self {
            angle_step_degrees: 10,
            max_simulation_ticks: 180,
            target_radius: 4.0,
            information_threshold: 0.05,
            information_weight: 1.0,
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
    pub reason: ShotReason,
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

        let max_velocity = energy.floor().min(8.0) as i32;
        let empty_map = PlanetMap { planets: vec![] };
        let map = state
            .map_belief
            .as_ref()
            .map(|belief| &belief.map)
            .unwrap_or(&empty_map);
        let angle_step = usize::from(self.settings.angle_step_degrees.max(1));
        let mut candidates = Vec::new();
        for velocity in 2..=max_velocity {
            let mut angles = (-180..=180).step_by(angle_step).collect::<Vec<_>>();
            if angles.last() != Some(&180) {
                angles.push(180);
            }
            for angle in angles {
                let shot = ShotObservation {
                    player_id: own_id,
                    angle_degrees: f64::from(angle),
                    velocity: f64::from(velocity),
                    points: vec![start.clone()],
                };
                let Ok(trajectory) = simulate_shot(
                    map,
                    &shot,
                    self.world,
                    self.settings.max_simulation_ticks.max(2),
                ) else {
                    continue;
                };
                let miss_distance = targets
                    .iter()
                    .map(|target| closest_path_distance(target, &trajectory.points))
                    .fold(f64::INFINITY, f64::min);
                candidates.push(ShotChoice {
                    velocity,
                    angle_degrees: angle,
                    miss_distance,
                    information_score: 0.0,
                    reason: ShotReason::Approach,
                });
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

        if let Some(best_hit) = pool
            .iter()
            .filter(|candidate| candidate.miss_distance <= self.settings.target_radius)
            .min_by(|left, right| {
                left.miss_distance
                    .total_cmp(&right.miss_distance)
                    .then_with(|| left.velocity.cmp(&right.velocity))
            })
        {
            return Some(ShotChoice {
                reason: ShotReason::Hit,
                ..*best_hit
            });
        }

        for candidate in pool.iter_mut() {
            candidate.information_score = state
                .map_belief
                .as_ref()
                .map(|belief| {
                    let Ok(trajectory) = simulate_shot(
                        &belief.map,
                        &ShotObservation {
                            player_id: own_id,
                            angle_degrees: f64::from(candidate.angle_degrees),
                            velocity: f64::from(candidate.velocity),
                            points: vec![start.clone()],
                        },
                        self.world,
                        self.settings.max_simulation_ticks.max(2),
                    ) else {
                        return 0.0;
                    };
                    let uncertainty_sum = trajectory.points.iter().fold(0.0, |sum, point| {
                        sum + belief
                            .field_uncertainty(Vec2::new(f64::from(point.x), f64::from(point.y)))
                    });
                    uncertainty_sum / trajectory.points.len().max(1) as f64
                })
                .unwrap_or(0.0);
        }

        if let Some(best_information) = pool
            .iter()
            .filter(|candidate| candidate.information_score >= self.settings.information_threshold)
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

        pool.into_iter().min_by(|left, right| {
            left.miss_distance
                .total_cmp(&right.miss_distance)
                .then_with(|| left.velocity.cmp(&right.velocity))
        })
    }
}

fn closest_path_distance(target: &Vec2, points: &[Point]) -> f64 {
    points
        .windows(2)
        .map(|segment| {
            let start = Vec2::new(f64::from(segment[0].x), f64::from(segment[0].y));
            let end = Vec2::new(f64::from(segment[1].x), f64::from(segment[1].y));
            let delta = Vec2::new(end.x - start.x, end.y - start.y);
            let offset = Vec2::new(target.x - start.x, target.y - start.y);
            let length_squared = delta.norm_squared();
            let fraction = if length_squared == 0.0 {
                0.0
            } else {
                ((offset.x * delta.x + offset.y * delta.y) / length_squared).clamp(0.0, 1.0)
            };
            let closest = Vec2::new(start.x + fraction * delta.x, start.y + fraction * delta.y);
            Vec2::new(target.x - closest.x, target.y - closest.y)
                .norm_squared()
                .sqrt()
        })
        .fold(f64::INFINITY, f64::min)
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
    fn policy_falls_back_to_known_shots_when_every_candidate_is_known() {
        let mut state = flat_field_state();
        for angle in (-180..=180).step_by(10) {
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
