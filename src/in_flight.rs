//! Shots in flight: whom they will hit and when, and the doomed-target race rule.
//!
//! NewtonWars announces a shot with `ShotBegin` (shooter, angle, velocity) and streams no
//! positions until `ShotFin`. A shot launched from a known position is simulated on the current
//! map belief to predict whom it kills and when. Targeting uses that to avoid aiming at a player
//! another shot (ours included) will kill before our shot arrives.

use crate::estimator::ResidualCorrection;
use crate::map::{PlanetMap, Vec2};
use crate::protocol::Point;
use crate::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_corrected_targeting_shot,
};

/// Server steps per second (one path point per step) until measured from this connection's
/// begin/finish pairs. Local servers run about 30.5 (captures 53-58); upstream nominally 60.
pub const DEFAULT_STEP_RATE: f64 = 30.5;
/// Shot slots per player on the server (`numShots`, default 16): the 17th shot of a player
/// reuses the slot of its 16th-last one, which silently drops that shot if still in flight.
pub const SHOT_SLOTS: u64 = 16;
/// Delay from a targeting decision to our shot's launch (commands, one server step).
pub const FIRE_LATENCY_MS: f64 = 60.0;

/// A shot that began but has not finished yet.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct InFlightShot {
    pub player_id: u32,
    /// The shooter's tracked position when the begin event arrived (`None` when unknown).
    pub launch: Option<Point>,
    pub velocity: f64,
    pub angle_degrees: f64,
    /// Local receive time of `ShotBegin` (unix ms); `None` for shots observed without a clock.
    #[serde(default)]
    pub begun_ms: Option<i64>,
    /// The shooter's begin count when this shot began (slot reuse, see `SHOT_SLOTS`).
    #[serde(default)]
    pub ordinal: u64,
}

/// A player an in-flight shot is predicted to kill.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PredictedKill {
    pub shooter: u32,
    pub victim: u32,
    /// Predicted impact time (unix ms, local clock).
    pub impact_ms: f64,
    /// Predicted ticks from launch to impact.
    pub flight_ticks: usize,
    /// Heuristic chance that the prediction comes true (`kill_probability`).
    pub probability: f64,
}

/// Chance that a shot our map predicts to hit really kills, by predicted flight ticks: map error
/// grows along the path. Calibrated on captures 53-57 (in-flight shots predicted to hit on the
/// recorded belief, outcome from the recorded events; pooled 0-59 ticks 66 of 89, 60-79 17 of
/// 38, 80-99 9 of 28, 100+ 11 of 67). A heuristic, not a calibrated posterior.
pub fn kill_probability(flight_ticks: usize) -> f64 {
    match flight_ticks {
        0..=59 => 0.75,
        60..=79 => 0.45,
        80..=99 => 0.3,
        _ => 0.15,
    }
}

/// Predict whom each in-flight shot kills, for impacts still in the future at `now_ms`.
/// Shots with an unknown launch or begin time are skipped (they cannot be placed). Players are
/// the latest known positions; a collision the map predicts for the past did not happen, so
/// that player is ignored for the shot.
#[allow(clippy::too_many_arguments)]
pub fn predict_kills(
    map: &PlanetMap,
    correction: Option<&ResidualCorrection>,
    shots: &[InFlightShot],
    players: &[(u32, Vec2)],
    now_ms: i64,
    step_rate: f64,
    world: WorldParameters,
    horizon_ticks: usize,
    player_radius: f64,
) -> Vec<PredictedKill> {
    let ms_per_tick = 1000.0 / step_rate;
    shots
        .iter()
        .filter_map(|shot| {
            let launch = shot.launch.clone()?;
            let begun_ms = shot.begun_ms?;
            let elapsed_ticks = ((now_ms - begun_ms).max(0) as f64 / ms_per_tick).floor() as usize;
            let observation = ShotObservation {
                player_id: shot.player_id,
                angle_degrees: shot.angle_degrees,
                velocity: shot.velocity,
                points: vec![launch],
            };
            // A collision on the part of the path already flown cannot have happened (the shot
            // is still in flight): the player arrived there later, e.g. respawned. Drop that
            // player and simulate again, a few times at most.
            let mut players = players.to_vec();
            for _ in 0..3 {
                let trajectory = simulate_corrected_targeting_shot(
                    map,
                    correction,
                    &observation,
                    world,
                    (elapsed_ticks + horizon_ticks + 2).min(2000),
                    &players,
                    player_radius,
                )
                .ok()?;
                let Termination::PlayerCollision(victim) = trajectory.termination else {
                    return None;
                };
                let flight_ticks = trajectory.points.len() - 1;
                let impact_ms = begun_ms as f64 + flight_ticks as f64 * ms_per_tick;
                if impact_ms > now_ms as f64 {
                    return Some(PredictedKill {
                        shooter: shot.player_id,
                        victim,
                        impact_ms,
                        flight_ticks,
                        probability: kill_probability(flight_ticks),
                    });
                }
                if victim == shot.player_id {
                    // Without the shooter the simulation never leaves the source.
                    return None;
                }
                players.retain(|(id, _)| *id != victim);
            }
            None
        })
        .collect()
}

/// Chance that `victim` is killed by one of `kills` before `our_impact_ms` (the race rule:
/// kills predicted after our impact do not count). Independent shots combine as
/// 1 - prod(1 - p).
pub fn doom_probability(kills: &[PredictedKill], victim: u32, our_impact_ms: f64) -> f64 {
    1.0 - kills
        .iter()
        .filter(|kill| kill.victim == victim && kill.impact_ms < our_impact_ms)
        .map(|kill| 1.0 - kill.probability)
        .product::<f64>()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kill(victim: u32, impact_ms: f64, probability: f64) -> PredictedKill {
        PredictedKill {
            shooter: 7,
            victim,
            impact_ms,
            flight_ticks: 30,
            probability,
        }
    }

    #[test]
    fn a_victim_is_doomed_only_by_kills_that_land_before_our_shot() {
        let kills = [kill(2, 1_000.0, 0.8), kill(3, 500.0, 0.6)];
        // Our shot lands after the in-flight one: the target will already be gone.
        assert!((doom_probability(&kills, 2, 1_200.0) - 0.8).abs() < 1e-12);
        // Our shot lands first: we win the race and the target is not doomed for us.
        assert_eq!(doom_probability(&kills, 2, 900.0), 0.0);
        // Other victims' threats do not count.
        assert_eq!(doom_probability(&[kill(3, 500.0, 0.6)], 2, 2_000.0), 0.0);
        // Two independent threats combine.
        let both = [kill(2, 100.0, 0.5), kill(2, 200.0, 0.5)];
        assert!((doom_probability(&both, 2, 1_000.0) - 0.75).abs() < 1e-12);
    }

    fn straight_shot(begun_ms: Option<i64>) -> InFlightShot {
        InFlightShot {
            player_id: 1,
            launch: Some(Point { x: 100.0, y: 100.0 }),
            velocity: 10.0,
            angle_degrees: 0.0,
            begun_ms,
            ordinal: 1,
        }
    }

    #[test]
    fn an_in_flight_shot_is_predicted_to_kill_the_player_on_its_path_at_its_impact_time() {
        let players = [(1, Vec2::new(100.0, 100.0)), (2, Vec2::new(300.0, 100.0))];
        let empty = PlanetMap { planets: vec![] };
        let predict = |shot: InFlightShot, now_ms: i64| {
            predict_kills(
                &empty,
                None,
                &[shot],
                &players,
                now_ms,
                30.0,
                WorldParameters::default(),
                180,
                4.0,
            )
        };
        // 10 units per tick, the player's disk starts 196 units out: impact at tick 20.
        let kills = predict(straight_shot(Some(0)), 100);
        assert_eq!(kills.len(), 1);
        assert_eq!((kills[0].shooter, kills[0].victim), (1, 2));
        assert_eq!(kills[0].flight_ticks, 20);
        assert!((kills[0].impact_ms - 20.0 * 1000.0 / 30.0).abs() < 1e-9);

        // A player who respawned onto the part already flown does not stop the prediction.
        let respawned = [
            (1, Vec2::new(100.0, 100.0)),
            (3, Vec2::new(150.0, 100.0)),
            (2, Vec2::new(300.0, 100.0)),
        ];
        let kills = predict_kills(
            &empty,
            None,
            &[straight_shot(Some(0))],
            &respawned,
            300,
            30.0,
            WorldParameters::default(),
            180,
            4.0,
        );
        assert_eq!(
            kills.iter().map(|kill| kill.victim).collect::<Vec<_>>(),
            [2]
        );

        // After the predicted impact the shot no longer threatens anyone.
        assert!(predict(straight_shot(Some(0)), 1_000).is_empty());
        // A shot without a begin time cannot be placed in time.
        assert!(predict(straight_shot(None), 100).is_empty());
    }
}
