//! Cheap evaluation helpers shared by the offline tools. They never fit anything.
use super::Shot;
use super::fit::{Fit, theta_of};
use crate::event_store::StoredEvent;
use crate::map::PlanetMap;
use crate::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use crate::simulation::{
    ShotObservation, WorldParameters, simulate_sampled_positions, simulate_shot,
};
use std::sync::atomic::AtomicBool;

/// The exact likelihood's chi2 of `planets` on every row of the paths (measurement floor only,
/// no model error, no contacts) and the number of rows (both axes). `None` when the paths have no
/// rows or the map is not evaluable. For comparing maps on the same paths (a ratio against the
/// truth's chi2), not as an absolute fit statistic.
pub fn exact_chi2(observations: &[ShotObservation], planets: &[Planet]) -> Option<(f64, usize)> {
    let world = WorldParameters::default();
    let shots: Vec<Shot> = observations
        .iter()
        .map(|obs| Shot::new(obs.clone(), None, world))
        .collect();
    let rows: usize = shots
        .iter()
        .filter_map(|shot| shot.path.as_ref())
        .map(|path| 2 * path.row_count())
        .sum();
    if rows == 0 {
        return None;
    }
    let never = AtomicBool::new(false);
    let theta: Vec<[f64; 3]> = planets.iter().map(theta_of).collect();
    let chi2 = Fit::new(&shots, world, &never, false, vec![]).chi2(&theta)?;
    Some((chi2, rows))
}

/// A deterministic synthetic world (the W7 generator of the diagnosis harnesses): `k`
/// non-overlapping planets with radius 20-40 and mass radius^3 / 10, and `paths` simulated paths
/// of at least 40 points from random launch points at speed 8, all by player 1. Seeds give the
/// same world on every machine.
pub fn synthetic_world(seed: u64, k: usize, paths: usize) -> (Vec<Planet>, Vec<ShotObservation>) {
    fn uniform(state: &mut u64) -> f64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 11) as f64 / (1u64 << 53) as f64
    }
    let world = WorldParameters::default();
    let mut state = seed;
    let mut truth: Vec<Planet> = Vec::new();
    while truth.len() < k {
        let radius = 20.0 + 20.0 * uniform(&mut state);
        let c = Planet {
            x: radius + (world.battlefield_width - 2.0 * radius) * uniform(&mut state),
            y: radius + (world.battlefield_height - 2.0 * radius) * uniform(&mut state),
            radius,
            mass: radius.powi(3) / 10.0,
        };
        if truth
            .iter()
            .all(|p| (p.x - c.x).hypot(p.y - c.y) > p.radius + c.radius)
        {
            truth.push(c);
        }
    }
    let map = PlanetMap {
        planets: truth.clone(),
    };
    let mut shots = Vec::new();
    while shots.len() < paths {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: (360.0 * uniform(&mut state)).floor(),
            velocity: 8.0,
            points: vec![Point {
                x: (100.0 + (world.battlefield_width - 200.0) * uniform(&mut state)) as f32,
                y: (100.0 + (world.battlefield_height - 200.0) * uniform(&mut state)) as f32,
            }],
        };
        let path = simulate_shot(&map, &shot, world, 2000).unwrap();
        if path.points.len() < 40 {
            continue;
        }
        shots.push(ShotObservation {
            points: path.points,
            ..shot
        });
    }
    (truth, shots)
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

fn rmse(squared_error: f64, coordinate_count: usize) -> f64 {
    (squared_error / coordinate_count as f64).sqrt()
}

/// Coordinate RMSE over all regular samples of several trajectories.
pub fn position_rmse(
    map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
) -> Result<f64, String> {
    let mut squared_error = 0.0;
    let mut coordinate_count = 0;
    for shot in observations {
        if let Some((shot_squared_error, shot_coordinate_count)) =
            shot_position_squared_error(map, shot, world)?
        {
            squared_error += shot_squared_error;
            coordinate_count += shot_coordinate_count;
        }
    }
    if coordinate_count == 0 {
        return Err("validation shots contain no regular positions".to_owned());
    }
    Ok(rmse(squared_error, coordinate_count))
}

/// Position RMSE for one completed trajectory, excluding its untimed terminal sample. `None`
/// when the trajectory has no regular samples to compare.
pub fn shot_position_rmse(
    map: &PlanetMap,
    observation: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<f64>, String> {
    Ok(shot_position_squared_error(map, observation, world)?
        .map(|(squared_error, count)| rmse(squared_error, count)))
}

fn shot_position_squared_error(
    map: &PlanetMap,
    shot: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<(f64, usize)>, String> {
    let usable_samples = shot.points.len().saturating_sub(1);
    if usable_samples < 2 {
        return Ok(None);
    }
    let predicted = simulate_sampled_positions(map, shot, world, usable_samples)?;
    let mut squared_error = 0.0;
    let mut coordinate_count = 0;
    for (observed, predicted) in shot.points[1..usable_samples].iter().zip(&predicted[1..]) {
        squared_error += (f64::from(observed.x) - predicted.x).powi(2);
        squared_error += (f64::from(observed.y) - predicted.y).powi(2);
        coordinate_count += 2;
    }
    Ok(Some((squared_error, coordinate_count)))
}
