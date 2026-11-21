use crate::field_hypothesis::FieldHypothesis;
use crate::local_field::LocalField;
use crate::map::{PlanetMap, Vec2};
use crate::protocol::{Point, ServerEvent};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WorldParameters {
    pub battlefield_width: f64,
    pub battlefield_height: f64,
    pub margin: f64,
    pub substeps_per_tick: usize,
}

impl Default for WorldParameters {
    fn default() -> Self {
        const AREA: f64 = 2_000_000.0;
        Self {
            battlefield_width: (AREA * 16.0 / 9.0).sqrt(),
            battlefield_height: (AREA * 9.0 / 16.0).sqrt(),
            margin: 500.0,
            substeps_per_tick: 25,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ShotObservation {
    pub player_id: u32,
    pub angle_degrees: f64,
    pub velocity: f64,
    pub points: Vec<Point>,
}

impl TryFrom<&ServerEvent> for ShotObservation {
    type Error = &'static str;

    fn try_from(event: &ServerEvent) -> Result<Self, Self::Error> {
        match event {
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                points,
            } => Ok(Self {
                player_id: *player_id,
                angle_degrees: *angle,
                velocity: *velocity,
                points: points.clone(),
            }),
            _ => Err("event is not a completed shot with launch parameters"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Termination {
    PlanetCollision(usize),
    PlayerCollision(u32),
    BattlefieldMargin,
    SampleLimit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PredictedTrajectory {
    pub points: Vec<Point>,
    pub termination: Termination,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PositionResidual {
    pub observed: Vec2,
    pub predicted: Vec2,
    pub dx: f64,
    pub dy: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TrajectoryResiduals {
    pub samples: Vec<PositionResidual>,
    pub rmse: f64,
    pub unmatched_observed: usize,
    pub unmatched_predicted: usize,
}

pub fn simulate_shot(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
    max_samples: usize,
) -> Result<PredictedTrajectory, String> {
    simulate_shot_with_players(map, shot, world, max_samples, &[], 0.0, None, None)
}

/// Simulate collisions against the latest known player positions, in server slot order.
/// Positions are snapshots: the server may move players while the shot is in flight.
pub fn simulate_targeting_shot(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
    max_samples: usize,
    players: &[(u32, Vec2)],
    player_radius: f64,
) -> Result<PredictedTrajectory, String> {
    simulate_shot_with_players(
        map,
        shot,
        world,
        max_samples,
        players,
        player_radius,
        None,
        None,
    )
}

/// Target using a locally observed residual field in addition to the physical planets.
pub fn simulate_targeting_shot_with_local_field(
    map: &PlanetMap,
    field: &LocalField,
    shot: &ShotObservation,
    world: WorldParameters,
    max_samples: usize,
    players: &[(u32, Vec2)],
    player_radius: f64,
) -> Result<PredictedTrajectory, String> {
    simulate_shot_with_players(
        map,
        shot,
        world,
        max_samples,
        players,
        player_radius,
        Some(field),
        None,
    )
}

/// Integrate an additional field hypothesis without giving it collision geometry.
/// The local field must contain residuals relative to both the physical map and hypothesis.
pub fn simulate_targeting_shot_with_hypothesis(
    map: &PlanetMap,
    field: &LocalField,
    hypothesis: &FieldHypothesis,
    shot: &ShotObservation,
    world: WorldParameters,
    max_samples: usize,
    players: &[(u32, Vec2)],
    player_radius: f64,
) -> Result<PredictedTrajectory, String> {
    simulate_shot_with_players(
        map,
        shot,
        world,
        max_samples,
        players,
        player_radius,
        Some(field),
        Some(hypothesis),
    )
}

fn simulate_shot_with_players(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
    max_samples: usize,
    players: &[(u32, Vec2)],
    player_radius: f64,
    field: Option<&LocalField>,
    hypothesis: Option<&FieldHypothesis>,
) -> Result<PredictedTrajectory, String> {
    if shot.points.is_empty() {
        return Err("shot observation has no launch position".to_owned());
    }
    if world.substeps_per_tick == 0 {
        return Err("substeps per tick must be nonzero".to_owned());
    }

    let start = &shot.points[0];
    let mut position = Vec2::new(f64::from(start.x), f64::from(start.y));
    let angle = shot.angle_degrees.to_radians();
    let mut velocity = Vec2::new(shot.velocity * angle.cos(), -shot.velocity * angle.sin());
    let mut acceleration = field_acceleration(map, field, hypothesis, position)?;
    let mut points = vec![Point {
        x: position.x as f32,
        y: position.y as f32,
    }];
    let max_samples = max_samples.max(1);
    let mut left_source = false;

    while points.len() < max_samples {
        for _ in 0..world.substeps_per_tick {
            integrate_substep(
                map,
                &mut position,
                &mut velocity,
                &mut acceleration,
                world.substeps_per_tick,
                field,
                hypothesis,
            )?;

            if let Some((planet_index, _)) = map.planets.iter().enumerate().find(|(_, planet)| {
                let dx = planet.x - position.x;
                let dy = planet.y - position.y;
                dx * dx + dy * dy <= planet.radius * planet.radius
            }) {
                points.push(to_point(position));
                return Ok(PredictedTrajectory {
                    points,
                    termination: Termination::PlanetCollision(planet_index),
                });
            }

            for (player_id, player_position) in players {
                let distance_squared = (position.x - player_position.x).powi(2)
                    + (position.y - player_position.y).powi(2);
                if left_source && distance_squared <= player_radius * player_radius {
                    points.push(to_point(position));
                    return Ok(PredictedTrajectory {
                        points,
                        termination: Termination::PlayerCollision(*player_id),
                    });
                }
                if *player_id == shot.player_id && distance_squared > (player_radius + 1.0).powi(2)
                {
                    left_source = true;
                }
            }

            if position.x < -world.margin
                || position.x > world.battlefield_width + world.margin
                || position.y < -world.margin
                || position.y > world.battlefield_height + world.margin
            {
                points.push(to_point(position));
                return Ok(PredictedTrajectory {
                    points,
                    termination: Termination::BattlefieldMargin,
                });
            }
        }

        points.push(to_point(position));
    }

    Ok(PredictedTrajectory {
        points,
        termination: Termination::SampleLimit,
    })
}

/// Simulate full-tick samples without collision stops, for a fixed-dimensional fit residual.
///
/// Planet endpoints have unknown substep time in protocol data, so the fitter compares only
/// regular tick samples before the final observed point.
pub fn simulate_sampled_path(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
    sample_count: usize,
) -> Result<Vec<Point>, String> {
    Ok(simulate_sampled_positions(map, shot, world, sample_count)?
        .into_iter()
        .map(to_point)
        .collect())
}

/// Simulate regular tick positions in f64 for parameter fitting.
pub fn simulate_sampled_positions(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
    sample_count: usize,
) -> Result<Vec<Vec2>, String> {
    simulate_sampled_positions_impl(map, None, None, shot, world, sample_count)
}

pub fn simulate_sampled_positions_with_local_field(
    map: &PlanetMap,
    field: &LocalField,
    shot: &ShotObservation,
    world: WorldParameters,
    sample_count: usize,
) -> Result<Vec<Vec2>, String> {
    simulate_sampled_positions_impl(map, Some(field), None, shot, world, sample_count)
}

/// Integrate timed samples with a field-only hypothesis and its matching local residual.
pub fn simulate_sampled_positions_with_hypothesis(
    map: &PlanetMap,
    field: &LocalField,
    hypothesis: &FieldHypothesis,
    shot: &ShotObservation,
    world: WorldParameters,
    sample_count: usize,
) -> Result<Vec<Vec2>, String> {
    simulate_sampled_positions_impl(
        map,
        Some(field),
        Some(hypothesis),
        shot,
        world,
        sample_count,
    )
}

fn simulate_sampled_positions_impl(
    map: &PlanetMap,
    field: Option<&LocalField>,
    hypothesis: Option<&FieldHypothesis>,
    shot: &ShotObservation,
    world: WorldParameters,
    sample_count: usize,
) -> Result<Vec<Vec2>, String> {
    if shot.points.is_empty() {
        return Err("shot observation has no launch position".to_owned());
    }
    if world.substeps_per_tick == 0 {
        return Err("substeps per tick must be nonzero".to_owned());
    }

    let start = &shot.points[0];
    let mut position = Vec2::new(f64::from(start.x), f64::from(start.y));
    let angle = shot.angle_degrees.to_radians();
    let mut velocity = Vec2::new(shot.velocity * angle.cos(), -shot.velocity * angle.sin());
    let mut acceleration = field_acceleration(map, field, hypothesis, position)?;
    let mut points = vec![position];
    while points.len() < sample_count {
        for _ in 0..world.substeps_per_tick {
            integrate_substep(
                map,
                &mut position,
                &mut velocity,
                &mut acceleration,
                world.substeps_per_tick,
                field,
                hypothesis,
            )?;
        }
        points.push(position);
    }
    Ok(points)
}

pub fn compare_trajectories(observed: &[Point], predicted: &[Point]) -> TrajectoryResiduals {
    let compared = observed.len().min(predicted.len());
    let samples = observed
        .iter()
        .zip(predicted)
        .map(|(observed, predicted)| {
            let dx = f64::from(observed.x) - f64::from(predicted.x);
            let dy = f64::from(observed.y) - f64::from(predicted.y);
            PositionResidual {
                observed: Vec2::new(f64::from(observed.x), f64::from(observed.y)),
                predicted: Vec2::new(f64::from(predicted.x), f64::from(predicted.y)),
                dx,
                dy,
            }
        })
        .collect::<Vec<_>>();
    let squared_error = samples
        .iter()
        .map(|sample| sample.dx.powi(2) + sample.dy.powi(2))
        .sum::<f64>();
    let rmse = if compared == 0 {
        0.0
    } else {
        (squared_error / (2 * compared) as f64).sqrt()
    };
    TrajectoryResiduals {
        samples,
        rmse,
        unmatched_observed: observed.len().saturating_sub(compared),
        unmatched_predicted: predicted.len().saturating_sub(compared),
    }
}

/// Rough acceleration observations from full-tick samples.
///
/// The final point is omitted because NewtonWars may append it at the collision substep, rather
/// than at the end of a full tick. Its time interval is not present in the wire event.
pub fn reconstruct_acceleration(points: &[Point]) -> Vec<(Vec2, Vec2)> {
    points
        .get(..points.len().saturating_sub(1))
        .unwrap_or_default()
        .windows(3)
        .map(|window| {
            let before = Vec2::new(f64::from(window[0].x), f64::from(window[0].y));
            let current = Vec2::new(f64::from(window[1].x), f64::from(window[1].y));
            let after = Vec2::new(f64::from(window[2].x), f64::from(window[2].y));
            (
                current,
                Vec2::new(
                    after.x - 2.0 * current.x + before.x,
                    after.y - 2.0 * current.y + before.y,
                ),
            )
        })
        .collect()
}

fn to_point(point: Vec2) -> Point {
    Point {
        x: point.x as f32,
        y: point.y as f32,
    }
}

fn field_acceleration(
    map: &PlanetMap,
    field: Option<&LocalField>,
    hypothesis: Option<&FieldHypothesis>,
    position: Vec2,
) -> Result<Vec2, String> {
    let mut acceleration = map
        .acceleration(position)
        .map_err(|error| error.to_string())?;
    if let Some(hypothesis) = hypothesis {
        let extra = hypothesis
            .acceleration(position)
            .ok_or("singular field hypothesis")?;
        if !extra.x.is_finite() || !extra.y.is_finite() {
            return Err("nonfinite field hypothesis acceleration".to_owned());
        }
        acceleration.x += extra.x;
        acceleration.y += extra.y;
    }
    if let Some(residual) = field.and_then(|field| field.residual(position)) {
        acceleration.x += residual.x;
        acceleration.y += residual.y;
    }
    if hypothesis.is_some() && (!acceleration.x.is_finite() || !acceleration.y.is_finite()) {
        return Err("nonfinite combined acceleration".to_owned());
    }
    Ok(acceleration)
}

fn integrate_substep(
    map: &PlanetMap,
    position: &mut Vec2,
    velocity: &mut Vec2,
    acceleration: &mut Vec2,
    substeps_per_tick: usize,
    field: Option<&LocalField>,
    hypothesis: Option<&FieldHypothesis>,
) -> Result<(), String> {
    let dt = 1.0 / substeps_per_tick as f64;
    let next_position = Vec2::new(
        position.x + velocity.x * dt + acceleration.x * 0.5 * dt * dt,
        position.y + velocity.y * dt + acceleration.y * 0.5 * dt * dt,
    );
    if hypothesis.is_some() && (!next_position.x.is_finite() || !next_position.y.is_finite()) {
        return Err("nonfinite trajectory position".to_owned());
    }
    let next_acceleration = field_acceleration(map, field, hypothesis, next_position)?;
    velocity.x += (acceleration.x + next_acceleration.x) * 0.5 * dt;
    velocity.y += (acceleration.y + next_acceleration.y) * 0.5 * dt;
    *position = next_position;
    *acceleration = next_acceleration;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field_source(x: f64, y: f64, mass: f64) -> crate::field_hypothesis::FieldHypothesis {
        crate::field_hypothesis::FieldHypothesis {
            x,
            y,
            mass,
            support: vec![],
            support_paths: vec![],
            baseline_continuation_rmse: 1.0,
            continuation_rmse: 0.5,
            training_paths: 1,
            validation_paths: 1,
            relative_improvement: 0.5,
        }
    }

    #[test]
    fn field_hypothesis_bends_path_without_creating_an_obstacle() {
        let map = PlanetMap { planets: vec![] };
        let field = LocalField::default();
        let hypothesis = field_source(10.0, 1.0, 0.001);
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 2.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        let world = WorldParameters::default();
        let trajectory = simulate_targeting_shot_with_hypothesis(
            &map,
            &field,
            &hypothesis,
            &shot,
            world,
            11,
            &[],
            4.0,
        )
        .unwrap();
        assert_eq!(trajectory.termination, Termination::SampleLimit);
        assert!(trajectory.points.last().unwrap().y > 0.0);
        let sampled =
            simulate_sampled_positions_with_hypothesis(&map, &field, &hypothesis, &shot, world, 11)
                .unwrap();
        assert!(sampled.last().unwrap().y > 0.0);
    }

    #[test]
    fn singular_or_nonfinite_hypothesis_cannot_produce_a_trajectory() {
        let map = PlanetMap { planets: vec![] };
        let field = LocalField::default();
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 2.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        for hypothesis in [
            field_source(0.0, 0.0, 1.0),
            field_source(1.0, 0.0, f64::NAN),
        ] {
            assert!(
                simulate_targeting_shot_with_hypothesis(
                    &map,
                    &field,
                    &hypothesis,
                    &shot,
                    WorldParameters::default(),
                    10,
                    &[],
                    4.0
                )
                .is_err()
            );
            assert!(
                simulate_sampled_positions_with_hypothesis(
                    &map,
                    &field,
                    &hypothesis,
                    &shot,
                    WorldParameters::default(),
                    10
                )
                .is_err()
            );
        }
    }

    #[test]
    fn field_hypothesis_does_not_hide_physical_planet_collision() {
        let map = PlanetMap {
            planets: vec![crate::protocol::Planet {
                x: 10.0,
                y: 0.0,
                radius: 1.0,
                mass: 0.0,
            }],
        };
        let field = LocalField::default();
        let hypothesis = field_source(10.0, 100.0, 0.001);
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 2.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        let trajectory = simulate_targeting_shot_with_hypothesis(
            &map,
            &field,
            &hypothesis,
            &shot,
            WorldParameters::default(),
            11,
            &[],
            4.0,
        )
        .unwrap();
        assert_eq!(trajectory.termination, Termination::PlanetCollision(0));
    }

    #[test]
    fn targeting_stops_at_first_player_after_leaving_source() {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 10.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        let players = [
            (1, Vec2::new(0.0, 0.0)),
            (2, Vec2::new(20.0, 0.0)),
            (3, Vec2::new(40.0, 0.0)),
        ];
        let path = simulate_targeting_shot(
            &PlanetMap { planets: vec![] },
            &shot,
            WorldParameters::default(),
            10,
            &players,
            4.0,
        )
        .unwrap();
        assert_eq!(path.termination, Termination::PlayerCollision(2));
        assert_eq!(path.points.last().unwrap().x, 16.0);
    }

    #[test]
    fn targeting_checks_discrete_substeps_not_swept_segments() {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 20.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        let players = [(1, Vec2::new(0.0, 0.0)), (2, Vec2::new(10.0, 0.0))];
        let world = WorldParameters {
            substeps_per_tick: 1,
            ..WorldParameters::default()
        };
        let path = simulate_targeting_shot(
            &PlanetMap { planets: vec![] },
            &shot,
            world,
            2,
            &players,
            4.0,
        )
        .unwrap();
        assert_eq!(path.termination, Termination::SampleLimit);
    }

    #[test]
    fn targeting_stops_at_planet_before_reaching_player() {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 10.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };
        let map = PlanetMap {
            planets: vec![crate::protocol::Planet {
                x: 10.0,
                y: 0.0,
                radius: 1.0,
                mass: 0.0,
            }],
        };
        let players = [(1, Vec2::new(0.0, 0.0)), (2, Vec2::new(20.0, 0.0))];
        let path =
            simulate_targeting_shot(&map, &shot, WorldParameters::default(), 10, &players, 4.0)
                .unwrap();
        assert_eq!(path.termination, Termination::PlanetCollision(0));
    }

    #[test]
    fn empty_map_preserves_constant_velocity_and_exact_position_residuals() {
        let map = PlanetMap { planets: vec![] };
        let shot = ShotObservation {
            player_id: 2,
            angle_degrees: 0.0,
            velocity: 10.0,
            points: vec![Point { x: 100.0, y: 50.0 }],
        };
        let predicted = simulate_shot(&map, &shot, WorldParameters::default(), 3).unwrap();

        assert_eq!(predicted.termination, Termination::SampleLimit);
        assert_eq!(
            predicted.points,
            vec![
                Point { x: 100.0, y: 50.0 },
                Point { x: 110.0, y: 50.0 },
                Point { x: 120.0, y: 50.0 },
            ]
        );
        let residuals = compare_trajectories(&predicted.points, &predicted.points);
        assert_eq!(residuals.rmse, 0.0);
        assert_eq!(residuals.unmatched_observed, 0);
    }

    #[test]
    fn planet_collision_returns_the_substep_contact_as_terminal_sample() {
        let map = PlanetMap {
            planets: vec![crate::protocol::Planet {
                x: 5.0,
                y: 0.0,
                radius: 1.0,
                mass: 0.0,
            }],
        };
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 10.0,
            points: vec![Point { x: 0.0, y: 0.0 }],
        };

        let predicted = simulate_shot(&map, &shot, WorldParameters::default(), 10).unwrap();

        assert_eq!(predicted.termination, Termination::PlanetCollision(0));
        assert_eq!(predicted.points.len(), 2);
        assert_eq!(predicted.points[1], Point { x: 4.4, y: 0.0 });
    }

    #[test]
    fn acceleration_reconstruction_ignores_untimed_terminal_sample() {
        let points = [
            Point { x: 0.0, y: 0.0 },
            Point { x: 1.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 100.0, y: 0.0 },
        ];

        assert_eq!(
            reconstruct_acceleration(&points),
            vec![(Vec2::new(1.0, 0.0), Vec2::new(2.0, 0.0))]
        );
    }
}
