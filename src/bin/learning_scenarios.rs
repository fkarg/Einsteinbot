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
fn predict_belief(
    belief: &MapBelief,
    shot: &ShotObservation,
    count: usize,
    use_hypothesis: bool,
) -> Result<Vec<Point>, String> {
    let world = WorldParameters::default();
    let positions = if use_hypothesis {
        let (Some(hypothesis), Some(field)) =
            (&belief.field_hypothesis, &belief.hypothesis_local_field)
        else {
            return Err("selected hypothesis or matching residual field missing".into());
        };
        einsteinbot::simulation::simulate_sampled_positions_with_hypothesis(
            &belief.map,
            field,
            hypothesis,
            shot,
            world,
            count,
        )
    } else {
        einsteinbot::simulation::simulate_sampled_positions_with_local_field(
            &belief.map,
            &belief.local_field,
            shot,
            world,
            count,
        )
    }?;
    Ok(positions
        .into_iter()
        .map(|position| Point {
            x: position.x as f32,
            y: position.y as f32,
        })
        .collect())
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
        "  {label}: actual={} predicted={:?} angle={} speed={} true_termination={:?} closest_sample={:.2} search_ms={} used_field_hypothesis={} alternative_agrees={:?} hypothesis_uncertainty_proxy={:?} local_support_fraction={:?}",
        if hit { "HIT" } else { "MISS" },
        choice.reason,
        choice.angle_degrees,
        choice.velocity,
        path.termination,
        distance_to_path(&path.points, &target),
        elapsed,
        choice.used_field_hypothesis,
        choice.alternative_agrees,
        choice.hypothesis_uncertainty_proxy,
        choice.local_support_fraction
    );
    // Held-out comparison evaluates the same candidate's regular samples, ending at the
    // first truth collision. Exclude the launch and untimed terminal sample.
    let empty = PlanetMap { planets: vec![] };
    let assumed = belief.as_ref().map(|b| &b.map).unwrap_or(&empty);
    let regular_count = path.points.len().saturating_sub(1);
    if regular_count > 1 {
        let predicted = einsteinbot::simulation::simulate_sampled_path(
            assumed,
            &candidate,
            WorldParameters::default(),
            regular_count,
        )
        .unwrap();
        let residual = einsteinbot::simulation::compare_trajectories(
            &path.points[1..regular_count],
            &predicted[1..],
        );
        println!(
            "    held_out_regular_samples={} planet_only_coordinate_rmse={:.3}",
            regular_count - 1,
            residual.rmse
        );
        if let Some(belief) = &belief {
            let predicted = match predict_belief(
                belief,
                &candidate,
                regular_count,
                choice.used_field_hypothesis,
            ) {
                Ok(predicted) => predicted,
                Err(error) => {
                    println!("    selected_model_prediction_error={error}");
                    return;
                }
            };
            let residual = einsteinbot::simulation::compare_trajectories(
                &path.points[1..regular_count],
                &predicted[1..],
            );
            println!(
                "    held_out_regular_samples={} selected_model_coordinate_rmse={:.3}",
                regular_count - 1,
                residual.rmse
            );
        }
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
    // untimed endpoint and launch. The oracle only generates/scores truth, never fit input.
    let count = witness.points.len().saturating_sub(1);
    for (label, map) in [
        ("flat", PlanetMap { planets: vec![] }),
        (
            "planet_only",
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
        let error = einsteinbot::simulation::compare_trajectories(
            &witness.points[1..count],
            &predicted[1..],
        );
        println!(
            "  fixed_witness_{label}_coordinate_rmse={:.3} samples={}",
            error.rmse,
            count - 1
        );
    }
    if let Some(belief) = &learned {
        let use_hypothesis =
            belief.field_hypothesis.is_some() && belief.hypothesis_local_field.is_some();
        let predicted = predict_belief(belief, &shot(scenario.trial), count, use_hypothesis);
        if let Ok(predicted) = predicted {
            let error = einsteinbot::simulation::compare_trajectories(
                &witness.points[1..count],
                &predicted[1..],
            );
            println!(
                "  fixed_witness_full_belief_coordinate_rmse={:.3} samples={}",
                error.rmse,
                count - 1
            );
        } else {
            println!(
                "  fixed_witness_full_belief_prediction_error={}",
                predicted.unwrap_err()
            );
        }
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
            name: "flat_diagonal",
            truth: PlanetMap { planets: vec![] },
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(250.0, 700.0, 35, 8),
            target_tick: 60,
        },
        Scenario {
            name: "one_planet_distant_flyby",
            truth: one(),
            observations: vec![(launch(200.0, 200.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_closer_flyby",
            truth: one(),
            observations: vec![(launch(200.0, 380.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_reverse_flyby",
            truth: one(),
            observations: vec![(
                launch(1400.0, 600.0, 180, 8),
                Termination::BattlefieldMargin,
            )],
            trial: launch(1400.0, 650.0, 180, 8),
            target_tick: 90,
        },
        Scenario {
            name: "small_planet_single_contact",
            truth: PlanetMap {
                planets: vec![planet(600.0, 500.0, 20.0)],
            },
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 400.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "large_planet_single_contact",
            truth: PlanetMap {
                planets: vec![planet(600.0, 500.0, 40.0)],
            },
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 300.0, 0, 8),
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
            name: "two_planets_flybys_only",
            truth: two(),
            observations: vec![
                (launch(200.0, 100.0, 0, 8), Termination::BattlefieldMargin),
                (
                    launch(1500.0, 800.0, 180, 8),
                    Termination::BattlefieldMargin,
                ),
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 110,
        },
        Scenario {
            name: "two_planets_established_reverse",
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
            trial: launch(1500.0, 600.0, 180, 8),
            target_tick: 100,
        },
        Scenario {
            name: "three_planets_established_swerve",
            truth: PlanetMap {
                planets: vec![
                    planet(600.0, 500.0, 30.0),
                    planet(1000.0, 350.0, 35.0),
                    planet(1300.0, 700.0, 25.0),
                ],
            },
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
                (
                    launch(1300.0, 1000.0, 90, 8),
                    Termination::PlanetCollision(2),
                ),
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 130,
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
    fn selected_model_prediction_uses_chosen_hypothesis_mode() {
        let mut belief = MapBelief::new(PlanetMap { planets: vec![] }, None);
        belief.field_hypothesis = Some(einsteinbot::field_hypothesis::FieldHypothesis {
            x: 200.0,
            y: 120.0,
            mass: 2700.0,
            support: vec![],
            baseline_continuation_rmse: 2.0,
            continuation_rmse: 1.0,
        });
        belief.hypothesis_local_field = Some(Default::default());
        let trial = shot(Launch {
            x: 100.0,
            y: 100.0,
            angle: 0,
            speed: 5,
        });
        let local_only = predict_belief(&belief, &trial, 10, false).unwrap();
        let with_hypothesis = predict_belief(&belief, &trial, 10, true).unwrap();
        assert_eq!(local_only.last().unwrap().y, 100.0);
        assert!(with_hypothesis.last().unwrap().y > 100.0);
    }

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
    fn noncontact_flyby_helps_next_shot_approach_target() {
        let scenario = scenarios()
            .into_iter()
            .find(|case| case.name == "one_planet_noncontact_flyby")
            .unwrap();
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
        let trial = shot(scenario.trial);
        let witness =
            simulate_shot(&scenario.truth, &trial, WorldParameters::default(), 2000).unwrap();
        let target = witness.points[scenario.target_tick].clone();
        let start = &trial.points[0];
        state.observe(&position(1, start.x, start.y));
        state.observe(&position(2, target.x, target.y));
        let players = [
            (1, Vec2::new(f64::from(start.x), f64::from(start.y))),
            (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
        ];
        let closest = |state: &BotState| {
            let choice = TargetingPolicy::default().choose(state, 20.0).unwrap();
            let candidate = shot(Launch {
                angle: choice.angle_degrees,
                speed: choice.velocity,
                ..scenario.trial
            });
            let actual = simulate_targeting_shot(
                &scenario.truth,
                &candidate,
                WorldParameters::default(),
                2000,
                &players,
                4.0,
            )
            .unwrap();
            distance_to_path(&actual.points, &target)
        };
        let flat_distance = closest(&state);
        state.set_map_belief(estimator.belief().cloned());
        let learned_distance = closest(&state);
        assert!(
            learned_distance + 50.0 < flat_distance,
            "flat={flat_distance} learned={learned_distance}"
        );
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
