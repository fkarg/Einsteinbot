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
    let mut acceleration = map
        .acceleration(position)
        .map_err(|error| error.to_string())?;
    let mut points = vec![Point {
        x: position.x as f32,
        y: position.y as f32,
    }];
    let max_samples = max_samples.max(1);

    while points.len() < max_samples {
        for _ in 0..world.substeps_per_tick {
            let dt = 1.0 / world.substeps_per_tick as f64;
            let new_position = Vec2::new(
                position.x + velocity.x * dt + acceleration.x * 0.5 * dt * dt,
                position.y + velocity.y * dt + acceleration.y * 0.5 * dt * dt,
            );
            let new_acceleration = map
                .acceleration(new_position)
                .map_err(|error| error.to_string())?;
            velocity.x += (acceleration.x + new_acceleration.x) * 0.5 * dt;
            velocity.y += (acceleration.y + new_acceleration.y) * 0.5 * dt;
            position = new_position;
            acceleration = new_acceleration;

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

#[cfg(test)]
mod tests {
    use super::*;

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
