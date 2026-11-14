use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, ServerEvent};
use einsteinbot::strategy::{BotState, MapBelief, ShotReason, TargetingPolicy, TargetingSettings};
use nalgebra::DMatrix;

fn state_with_enemy(enemy_x: f32, enemy_y: f32) -> BotState {
    let mut state = BotState::default();
    state.observe(&ServerEvent::OwnId { player_id: 0 });
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 0,
        x: 0.0,
        y: 0.0,
    });
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 1,
        x: enemy_x,
        y: enemy_y,
    });
    state
}

#[test]
fn finds_a_legal_low_energy_shot_on_a_clear_target() {
    let state = state_with_enemy(20.0, 0.0);
    let policy = TargetingPolicy::new(TargetingSettings {
        angle_step_degrees: 5,
        max_simulation_ticks: 30,
        ..TargetingSettings::default()
    });

    let choice = policy.choose(&state, 2.9).unwrap();

    assert_eq!(choice.velocity, 2);
    assert_eq!(choice.angle_degrees, 0);
    assert_eq!(choice.reason, ShotReason::Hit);
    assert!(choice.miss_distance <= 4.0);
}

#[test]
fn uses_dummy_fallback_when_no_opponent_is_known() {
    let mut state = BotState::default();
    state.observe(&ServerEvent::OwnId { player_id: 0 });
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 0,
        x: 0.0,
        y: 0.0,
    });

    assert!(TargetingPolicy::default().choose(&state, 8.0).is_none());
}

#[test]
fn never_spends_information_shot_when_a_hit_is_available() {
    let mut state = state_with_enemy(20.0, 0.0);
    state.set_map_belief(Some(MapBelief::new(
        PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2_700.0,
            }],
        },
        Some(DMatrix::identity(3, 3) * 1e12),
    )));
    let policy = TargetingPolicy::new(TargetingSettings {
        information_threshold: 0.0,
        ..TargetingSettings::default()
    });

    let choice = policy.choose(&state, 8.0).unwrap();

    assert_eq!(choice.reason, ShotReason::Hit);
    assert!(choice.miss_distance <= 4.0);
}

#[test]
fn information_shot_requires_uncertainty_above_the_threshold() {
    let mut uncertain_state = state_with_enemy(1700.0, 1000.0);
    uncertain_state.set_map_belief(Some(MapBelief::new(
        PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2_700.0,
            }],
        },
        Some(DMatrix::identity(3, 3) * 1e12),
    )));
    let policy = TargetingPolicy::new(TargetingSettings {
        angle_step_degrees: 30,
        max_simulation_ticks: 20,
        information_threshold: 0.01,
        ..TargetingSettings::default()
    });

    let exploratory_choice = policy.choose(&uncertain_state, 8.0).unwrap();
    assert_eq!(exploratory_choice.reason, ShotReason::Information);

    uncertain_state.set_map_belief(Some(MapBelief::new(
        PlanetMap {
            planets: vec![Planet {
                x: 500.0,
                y: 500.0,
                radius: 30.0,
                mass: 2_700.0,
            }],
        },
        Some(DMatrix::identity(3, 3) * 1e-12),
    )));
    let conservative_choice = policy.choose(&uncertain_state, 8.0).unwrap();
    assert_ne!(conservative_choice.reason, ShotReason::Information);
}

#[test]
fn falls_back_when_energy_cannot_pay_for_a_shot() {
    let state = state_with_enemy(20.0, 0.0);
    assert!(TargetingPolicy::default().choose(&state, 1.99).is_none());
}

#[test]
fn covered_non_hit_corridor_is_penalized_and_reset_restores_it() {
    use einsteinbot::protocol::Point;
    // A very distant target cannot be hit in 30 ticks. The old policy retraces
    // its straight best approach even after observing that entire corridor.
    let mut state = state_with_enemy(1700.0, 0.0);
    let policy = TargetingPolicy::new(TargetingSettings {
        max_simulation_ticks: 30,
        ..TargetingSettings::default()
    });
    let first = policy.choose(&state, 2.0).unwrap();
    assert_eq!(first.angle_degrees, 0);
    state.observe(&ServerEvent::ShotFin {
        player_id: 9,
        angle: 1.0,
        velocity: 2.0,
        points: (0..=60)
            .map(|x| Point {
                x: x as f32,
                y: 0.0,
            })
            .collect(),
    });
    let next = policy.choose(&state, 2.0).unwrap();
    assert_ne!(next.angle_degrees, 0);
    assert_eq!(next.reason, ShotReason::Approach);
    state.observe(&ServerEvent::Planets { planets: vec![] });
    assert_eq!(policy.choose(&state, 2.0).unwrap().angle_degrees, 0);
}

#[test]
fn coverage_never_overrides_a_predicted_hit_and_unknown_covariance_is_reported() {
    use einsteinbot::protocol::Point;
    let mut state = state_with_enemy(20.0, 0.0);
    state.observe(&ServerEvent::ShotFin {
        player_id: 9,
        angle: 1.0,
        velocity: 2.0,
        points: (0..=30)
            .map(|x| Point {
                x: x as f32,
                y: 0.0,
            })
            .collect(),
    });
    let choice = TargetingPolicy::default().choose(&state, 2.0).unwrap();
    assert_eq!(choice.reason, ShotReason::Hit);
    assert_eq!(choice.angle_degrees, 0);
    assert!(!choice.uncertainty_available);
}

#[test]
fn invalid_covariance_is_reported_as_unavailable_even_for_hits() {
    let mut state = state_with_enemy(20.0, 0.0);
    state.set_map_belief(Some(MapBelief::new(
        PlanetMap { planets: vec![] },
        Some(DMatrix::identity(3, 3)),
    )));
    let choice = TargetingPolicy::default().choose(&state, 2.0).unwrap();
    assert_eq!(choice.reason, ShotReason::Hit);
    assert!(!choice.uncertainty_available);
}

fn planet_contact() -> ServerEvent {
    use einsteinbot::protocol::Point;
    use einsteinbot::simulation::{ShotObservation, WorldParameters, simulate_shot};
    let shot = ShotObservation {
        player_id: 0,
        angle_degrees: 0.0,
        velocity: 2.0,
        points: vec![Point { x: 100.0, y: 100.0 }],
    };
    let map = PlanetMap {
        planets: vec![Planet {
            x: 300.0,
            y: 100.0,
            radius: 30.0,
            mass: 2700.0,
        }],
    };
    let path = simulate_shot(&map, &shot, WorldParameters::default(), 180).unwrap();
    ServerEvent::ShotFin {
        player_id: 0,
        angle: 0.0,
        velocity: 2.0,
        points: path.points,
    }
}

fn learn_contact(state: &mut BotState) {
    use einsteinbot::online_map::OnlineEstimator;
    use std::sync::atomic::AtomicBool;
    let mut estimator = OnlineEstimator::default();
    let cancelled = AtomicBool::new(false);
    for (player_id, point) in state.player_positions() {
        estimator.observe(
            &ServerEvent::PlayerPosition {
                player_id: *player_id,
                x: point.x,
                y: point.y,
            },
            &cancelled,
        );
    }
    let event = planet_contact();
    state.observe(&event);
    state.set_map_belief(estimator.observe(&event, &cancelled));
}

#[test]
fn observed_planet_contact_discourages_a_false_nominal_hit_and_resets() {
    let mut state = state_with_enemy(500.0, 100.0);
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 0,
        x: 100.0,
        y: 100.0,
    });
    learn_contact(&mut state);
    let policy = TargetingPolicy::default();
    let choice = policy.choose(&state, 8.0).unwrap();
    assert_ne!(
        choice.angle_degrees, 0,
        "do not keep shooting through the observed planet"
    );
    state.observe(&ServerEvent::Planets { planets: vec![] });
    assert_eq!(policy.choose(&state, 8.0).unwrap().reason, ShotReason::Hit);
}

#[test]
fn planet_beyond_target_does_not_disqualify_a_hit() {
    let mut state = state_with_enemy(200.0, 100.0);
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 0,
        x: 100.0,
        y: 100.0,
    });
    learn_contact(&mut state);
    let choice = TargetingPolicy::default().choose(&state, 8.0).unwrap();
    assert_eq!(choice.reason, ShotReason::Hit);
    assert!(!choice.blocked_by_contact);
    assert!(choice.miss_distance <= 4.0);
}
