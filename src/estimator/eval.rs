//! Cheap evaluation helpers shared by the offline tools. They never fit anything.
use crate::event_store::StoredEvent;
use crate::map::PlanetMap;
use crate::protocol::{RecordedEvent, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters, simulate_sampled_positions};

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

/// Coordinate RMSE over all regular samples of several trajectories.
pub fn position_rmse(
    map: &PlanetMap,
    observations: &[ShotObservation],
    world: WorldParameters,
) -> Result<f64, String> {
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
        return Err("validation shots contain no regular positions".to_owned());
    }
    Ok((squared_error / coordinate_count as f64).sqrt())
}

/// Position RMSE for one completed trajectory, excluding its untimed terminal sample.
pub fn shot_position_rmse(
    map: &PlanetMap,
    observation: &ShotObservation,
    world: WorldParameters,
) -> Result<Option<f64>, String> {
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
