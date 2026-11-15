//! Deterministic offline learning experiments. Ground truth generates observations and scores
//! choices; it is never sent to OnlineEstimator or the learned-map targeting policy.
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::protocol::{Planet, Point, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_shot, simulate_targeting_shot,
};
use einsteinbot::strategy::{BotState, MapBelief, TargetingPolicy};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Clone, Copy)]
struct Launch {
    x: f32,
    y: f32,
    angle: i32,
    speed: i32,
}
struct Scenario {
    name: &'static str,
    truth: PlanetMap,
    observations: Vec<(Launch, Termination)>,
    trial: Launch,
    target_tick: usize,
}

fn planet(x: f64, y: f64, radius: f64) -> Planet {
    Planet {
        x,
        y,
        radius,
        mass: radius.powi(3) / 10.0,
    }
}
fn shot(launch: Launch) -> ShotObservation {
    ShotObservation {
        player_id: 1,
        angle_degrees: f64::from(launch.angle),
        velocity: f64::from(launch.speed),
        points: vec![Point {
            x: launch.x,
            y: launch.y,
        }],
    }
}
fn position(id: u32, x: f32, y: f32) -> ServerEvent {
    ServerEvent::PlayerPosition {
        player_id: id,
        x,
        y,
    }
}
fn observe_launch(
    estimator: &mut OnlineEstimator,
    state: &mut BotState,
    truth: &PlanetMap,
    launch: Launch,
    expected: Termination,
) {
    let source = position(1, launch.x, launch.y);
    estimator.observe(&source, &AtomicBool::new(false));
    state.observe(&source);
    // The observed source is the only player in the field during these training shots.
    // Every ShotFin below is a real planet or margin termination, never a truncated path.
    let path = simulate_targeting_shot(
        truth,
        &shot(launch),
        WorldParameters::default(),
        2000,
        &[(1, Vec2::new(f64::from(launch.x), f64::from(launch.y)))],
        4.0,
    )
    .unwrap();
    assert_eq!(
        path.termination, expected,
        "training fixture did not reach its intended termination"
    );
    let event = ServerEvent::ShotFin {
        player_id: 1,
        angle: f64::from(launch.angle),
        velocity: f64::from(launch.speed),
        points: path.points,
    };
    state.observe(&event);
    let started = Instant::now();
    let belief = estimator
        .observe(&event, &AtomicBool::new(false))
        .expect("new observation yields belief");
    println!(
        "  training angle={} speed={} termination={:?} samples={} fit_ms={} inferred_planets={} converged={:?} rmse={:?} covariance={}",
        launch.angle,
        launch.speed,
        path.termination,
        match &event {
            ServerEvent::ShotFin { points, .. } => points.len(),
            _ => unreachable!(),
        },
        started.elapsed().as_millis(),
        belief.map.planets.len(),
        belief.fit_converged,
        belief.model_position_rmse,
        belief.parameter_covariance.is_some()
    );
}
fn distance_to_path(path: &[Point], target: &Point) -> f64 {
    path.iter()
        .map(|p| f64::from(p.x - target.x).hypot(f64::from(p.y - target.y)))
        .fold(f64::INFINITY, f64::min)
}
fn evaluate(
    label: &str,
    state: &mut BotState,
    belief: Option<MapBelief>,
    truth: &PlanetMap,
    start: Point,
    target: Point,
) {
    state.set_map_belief(belief.clone());
    let started = Instant::now();
    let Some(choice) = TargetingPolicy::default().choose(state, 20.0) else {
        println!("  {label}: NO_CHOICE");
        return;
    };
    let elapsed = started.elapsed().as_millis();
    let candidate = ShotObservation {
        player_id: 1,
        angle_degrees: f64::from(choice.angle_degrees),
        velocity: f64::from(choice.velocity),
        points: vec![start.clone()],
    };
    let players = [
        (1, Vec2::new(f64::from(start.x), f64::from(start.y))),
        (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
    ];
    let path = simulate_targeting_shot(
        truth,
        &candidate,
        WorldParameters::default(),
        2000,
        &players,
        4.0,
    )
    .unwrap();
    let hit = path.termination == Termination::PlayerCollision(2);
    println!(
        "  {label}: actual={} predicted={:?} angle={} speed={} true_termination={:?} closest_sample={:.2} search_ms={}",
        if hit { "HIT" } else { "MISS" },
        choice.reason,
        choice.angle_degrees,
        choice.velocity,
        path.termination,
        distance_to_path(&path.points, &target),
        elapsed
    );
    // Held-out comparison evaluates the same candidate's regular samples, ending at the
    // first truth collision. The last sample is untimed and therefore excluded.
    let assumed = belief
        .map(|b| b.map)
        .unwrap_or(PlanetMap { planets: vec![] });
    let regular_count = path.points.len().saturating_sub(1);
    if regular_count > 1 {
        let predicted = einsteinbot::simulation::simulate_sampled_path(
            &assumed,
            &candidate,
            WorldParameters::default(),
            regular_count,
        )
        .unwrap();
        let residual = einsteinbot::simulation::compare_trajectories(
            &path.points[..regular_count],
            &predicted,
        );
        println!(
            "    held_out_regular_samples={} coordinate_rmse={:.3}",
            regular_count, residual.rmse
        );
    }
}
fn run(scenario: Scenario) {
    println!("scenario={}", scenario.name);
    let mut estimator = OnlineEstimator::default();
    let mut state = BotState::default();
    state.observe(&ServerEvent::OwnId { player_id: 1 });
    for (observation, expected) in scenario.observations {
        observe_launch(
            &mut estimator,
            &mut state,
            &scenario.truth,
            observation,
            expected,
        );
    }
    let trial_path = simulate_shot(
        &scenario.truth,
        &shot(scenario.trial),
        WorldParameters::default(),
        2000,
    )
    .unwrap();
    assert!(
        trial_path.points.len() > scenario.target_tick,
        "fixture has no target tick; termination {:?}",
        trial_path.termination
    );
    let start = Point {
        x: scenario.trial.x,
        y: scenario.trial.y,
    };
    let target = trial_path.points[scenario.target_tick].clone();
    state.observe(&position(1, start.x, start.y));
    state.observe(&position(2, target.x, target.y));
    println!(
        "  trial start=({:.1},{:.1}) target=({:.1},{:.1}) witness_angle={} witness_speed={} witness_tick={} true_planets={}",
        start.x,
        start.y,
        target.x,
        target.y,
        scenario.trial.angle,
        scenario.trial.speed,
        scenario.target_tick,
        scenario.truth.planets.len()
    );
    let witness = simulate_targeting_shot(
        &scenario.truth,
        &shot(scenario.trial),
        WorldParameters::default(),
        2000,
        &[
            (1, Vec2::new(f64::from(start.x), f64::from(start.y))),
            (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
        ],
        4.0,
    )
    .unwrap();
    assert_eq!(
        witness.termination,
        Termination::PlayerCollision(2),
        "invalid target witness"
    );
    let nearest = scenario
        .truth
        .planets
        .iter()
        .map(|planet| {
            trial_path.points[..=scenario.target_tick]
                .iter()
                .map(|point| (f64::from(point.x) - planet.x).hypot(f64::from(point.y) - planet.y))
                .fold(f64::INFINITY, f64::min)
        })
        .collect::<Vec<_>>();
    let angle = f64::from(scenario.trial.angle).to_radians();
    let straight_x = f64::from(start.x)
        + scenario.target_tick as f64 * f64::from(scenario.trial.speed) * angle.cos();
    let straight_y = f64::from(start.y)
        - scenario.target_tick as f64 * f64::from(scenario.trial.speed) * angle.sin();
    println!(
        "  witness_planet_center_distances={nearest:?} deflection_from_flat={:.2}",
        (f64::from(target.x) - straight_x).hypot(f64::from(target.y) - straight_y)
    );
    let learned = estimator.belief().cloned();
    if let Some(belief) = &learned {
        for estimated in &belief.map.planets {
            if let Some(actual) = scenario.truth.planets.iter().min_by(|a, b| {
                (a.x - estimated.x)
                    .hypot(a.y - estimated.y)
                    .total_cmp(&(b.x - estimated.x).hypot(b.y - estimated.y))
            }) {
                println!(
                    "  nearest_planet center_error={:.3} radius_error={:.3}",
                    (actual.x - estimated.x).hypot(actual.y - estimated.y),
                    (actual.radius - estimated.radius).abs()
                );
            } else {
                println!(
                    "  FALSE_POSITIVE_PLANET x={:.1} y={:.1} radius={:.1}",
                    estimated.x, estimated.y, estimated.radius
                );
            }
        }
    }
    // Identical fixed held-out witness for flat and learned maps, excluding the final
    // untimed endpoint. The oracle only generates/scorers truth, never fit input.
    let count = witness.points.len().saturating_sub(1);
    for (label, map) in [
        ("flat", PlanetMap { planets: vec![] }),
        (
            "learned",
            learned
                .as_ref()
                .map(|b| b.map.clone())
                .unwrap_or(PlanetMap { planets: vec![] }),
        ),
    ] {
        let predicted = einsteinbot::simulation::simulate_sampled_path(
            &map,
            &shot(scenario.trial),
            WorldParameters::default(),
            count,
        )
        .unwrap();
        let error =
            einsteinbot::simulation::compare_trajectories(&witness.points[..count], &predicted);
        println!(
            "  fixed_witness_{label}_coordinate_rmse={:.3} samples={count}",
            error.rmse
        );
    }
    evaluate(
        "flat",
        &mut state,
        None,
        &scenario.truth,
        start.clone(),
        target.clone(),
    );
    evaluate(
        "learned",
        &mut state,
        learned,
        &scenario.truth,
        start.clone(),
        target.clone(),
    );
    evaluate(
        "oracle",
        &mut state,
        Some(MapBelief::new(scenario.truth.clone(), None)),
        &scenario.truth,
        start,
        target,
    );
}
fn scenarios() -> Vec<Scenario> {
    let launch = |x, y, angle, speed| Launch { x, y, angle, speed };
    let one = || PlanetMap {
        planets: vec![planet(600.0, 500.0, 30.0)],
    };
    let two = || PlanetMap {
        planets: vec![planet(600.0, 500.0, 30.0), planet(1000.0, 350.0, 35.0)],
    };
    vec![
        Scenario {
            name: "flat_direct",
            truth: PlanetMap { planets: vec![] },
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 250.0, 0, 8),
            target_tick: 50,
        },
        Scenario {
            name: "one_planet_noncontact_flyby",
            truth: one(),
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_single_contact",
            truth: one(),
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_established",
            truth: one(),
            observations: vec![
                (launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0)),
                (
                    launch(600.0, 100.0, -90, 8),
                    Termination::PlanetCollision(0),
                ),
                (
                    launch(1000.0, 500.0, 180, 8),
                    Termination::PlanetCollision(0),
                ),
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "two_planets_one_known",
            truth: two(),
            observations: vec![
                (launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0)),
                (
                    launch(600.0, 100.0, -90, 8),
                    Termination::PlanetCollision(0),
                ),
                (launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin),
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 110,
        },
        Scenario {
            name: "two_planets_established",
            truth: two(),
            observations: vec![
                (launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0)),
                (
                    launch(600.0, 100.0, -90, 8),
                    Termination::PlanetCollision(0),
                ),
                (
                    launch(1000.0, 100.0, -90, 8),
                    Termination::PlanetCollision(1),
                ),
                (
                    launch(1500.0, 350.0, 180, 8),
                    Termination::PlanetCollision(1),
                ),
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 110,
        },
    ]
}

fn main() {
    for scenario in scenarios() {
        run(scenario);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn training_and_witness_paths_are_physically_valid() {
        for scenario in scenarios() {
            for (launch, expected) in scenario.observations {
                let players = [(1, Vec2::new(f64::from(launch.x), f64::from(launch.y)))];
                let trajectory = simulate_targeting_shot(
                    &scenario.truth,
                    &shot(launch),
                    WorldParameters::default(),
                    2000,
                    &players,
                    4.0,
                )
                .unwrap();
                assert_eq!(
                    trajectory.termination, expected,
                    "{} training path",
                    scenario.name
                );
            }
            let witness = simulate_shot(
                &scenario.truth,
                &shot(scenario.trial),
                WorldParameters::default(),
                2000,
            )
            .unwrap();
            let target = &witness.points[scenario.target_tick];
            let players = [
                (
                    1,
                    Vec2::new(f64::from(scenario.trial.x), f64::from(scenario.trial.y)),
                ),
                (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
            ];
            let trajectory = simulate_targeting_shot(
                &scenario.truth,
                &shot(scenario.trial),
                WorldParameters::default(),
                2000,
                &players,
                4.0,
            )
            .unwrap();
            assert_eq!(
                trajectory.termination,
                Termination::PlayerCollision(2),
                "{} witness",
                scenario.name
            );
        }
    }

    #[test]
    fn flat_training_and_target_are_handled_without_inventing_a_planet() {
        let scenario = scenarios().remove(0);
        let mut estimator = OnlineEstimator::default();
        let mut state = BotState::default();
        state.observe(&ServerEvent::OwnId { player_id: 1 });
        for (launch, expected) in scenario.observations {
            observe_launch(
                &mut estimator,
                &mut state,
                &scenario.truth,
                launch,
                expected,
            );
        }
        assert!(estimator.belief().unwrap().map.planets.is_empty());
        let start = scenario.trial;
        state.observe(&position(1, start.x, start.y));
        state.observe(&position(2, 600.0, 250.0));
        let choice = TargetingPolicy::default().choose(&state, 20.0).unwrap();
        let players = [(1, Vec2::new(200.0, 250.0)), (2, Vec2::new(600.0, 250.0))];
        let selected = shot(Launch {
            angle: choice.angle_degrees,
            speed: choice.velocity,
            ..start
        });
        let trajectory = simulate_targeting_shot(
            &scenario.truth,
            &selected,
            WorldParameters::default(),
            2000,
            &players,
            4.0,
        )
        .unwrap();
        assert_eq!(trajectory.termination, Termination::PlayerCollision(2));
    }
}
