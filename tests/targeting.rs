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
