//! Deterministic offline learning experiments. Ground truth generates observations and scores
//! choices; it is never sent to Estimator or the learned-map targeting policy.
use einsteinbot::estimator::Estimator;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_shot, simulate_targeting_shot,
};
use einsteinbot::strategy::{BotState, MapBelief, TargetingPolicy};
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[path = "learning_scenarios/scenario.rs"]
mod scenario;
#[path = "learning_scenarios/w7.rs"]
mod w7;
use scenario::{Launch, Scenario, ScenarioCapture};

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
    estimator: &mut Estimator,
    state: &mut BotState,
    truth: &PlanetMap,
    launch: Launch,
    expected: Termination,
    mut capture: Option<&mut ScenarioCapture>,
    scenario: &Scenario,
    stage: usize,
) {
    let source = position(1, launch.x, launch.y);
    if let Some(capture) = capture.as_deref_mut() {
        capture.event(&source, None);
        capture.event(
            &ServerEvent::ShotBegin {
                player_id: 1,
                angle: f64::from(launch.angle),
                velocity: f64::from(launch.speed),
            },
            Some("scenario"),
        );
    }
    estimator.observe(&source, &AtomicBool::new(false));
    state.set_player_position(
        1,
        Point {
            x: launch.x,
            y: launch.y,
        },
    );
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
    let never = AtomicBool::new(false);
    let local = estimator
        .observe(&event, &never)
        .expect("new observation yields belief");
    let belief = estimator.settle(&never).unwrap_or(local);
    if let Some(capture) = capture {
        capture.event(&event, Some("scenario"));
        capture.decision(
            scenario,
            state,
            Some(&belief),
            None,
            &format!("training_{stage}"),
            stage,
            Some(started.elapsed().as_millis()),
        );
    }
    println!(
        "  training angle={} speed={} termination={:?} samples={} fit_ms={} inferred_planets={} map_weight={:?} rmse={:?} covariance={}",
        launch.angle,
        launch.speed,
        path.termination,
        match &event {
            ServerEvent::ShotFin { points, .. } => points.len(),
            _ => unreachable!(),
        },
        started.elapsed().as_millis(),
        belief.map.planets.len(),
        belief.map_weight,
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
) -> Option<einsteinbot::strategy::ShotChoice> {
    state.set_map_belief(belief.clone());
    let started = Instant::now();
    let Some(choice) = TargetingPolicy::default().choose(state, 20.0) else {
        println!("  {label}: NO_CHOICE");
        return None;
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
    }
    Some(choice)
}

fn run(scenario: Scenario, mut capture: Option<ScenarioCapture>) {
    println!("scenario={}", scenario.name);
    let mut estimator = Estimator::default();
    let mut state = BotState::default();
    if let Some(capture) = &mut capture {
        capture.event(
            &ServerEvent::Planets {
                planets: scenario.truth.planets.clone(),
            },
            None,
        );
    }
    state.observe(&ServerEvent::OwnId { player_id: 1 });
    if let Some(capture) = &mut capture {
        capture.event(&ServerEvent::OwnId { player_id: 1 }, Some("scenario"));
    }
    for (stage, (observation, expected)) in scenario.observations.iter().copied().enumerate() {
        observe_launch(
            &mut estimator,
            &mut state,
            &scenario.truth,
            observation,
            expected,
            capture.as_mut(),
            &scenario,
            stage + 1,
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
    state.set_player_position(1, start.clone());
    state.set_player_position(2, target.clone());
    if let Some(capture) = &mut capture {
        capture.event(&position(1, start.x, start.y), None);
        capture.event(&position(2, target.x, target.y), None);
        capture.event(
            &ServerEvent::OwnEnergy {
                player_id: 1,
                energy: 20.0,
            },
            Some("scenario"),
        );
    }
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
    evaluate(
        "flat",
        &mut state,
        None,
        &scenario.truth,
        start.clone(),
        target.clone(),
    );
    let learned_choice = evaluate(
        "learned",
        &mut state,
        learned.clone(),
        &scenario.truth,
        start.clone(),
        target.clone(),
    );
    if let Some(capture) = &mut capture {
        capture.decision(
            &scenario,
            &state,
            learned.as_ref(),
            learned_choice,
            "target",
            scenario.observations.len(),
            None,
        );
        if let Some(choice) = learned_choice {
            let trial = ShotObservation {
                player_id: 1,
                angle_degrees: f64::from(choice.angle_degrees),
                velocity: f64::from(choice.velocity),
                points: vec![start.clone()],
            };
            let players = [
                (1, Vec2::new(f64::from(start.x), f64::from(start.y))),
                (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
            ];
            if let Ok(path) = simulate_targeting_shot(
                &scenario.truth,
                &trial,
                WorldParameters::default(),
                2000,
                &players,
                4.0,
            ) {
                capture.event(
                    &ServerEvent::ShotBegin {
                        player_id: 1,
                        angle: trial.angle_degrees,
                        velocity: trial.velocity,
                    },
                    Some("scenario"),
                );
                capture.event(
                    &ServerEvent::ShotFin {
                        player_id: 1,
                        angle: trial.angle_degrees,
                        velocity: trial.velocity,
                        points: path.points,
                    },
                    Some("scenario"),
                );
            }
        }
    }
    evaluate(
        "oracle",
        &mut state,
        Some(MapBelief::new(scenario.truth.clone(), None)),
        &scenario.truth,
        start,
        target,
    );
    if let Some(capture) = capture {
        capture.finish();
    }
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
            family: "flat_baseline",
            defect_class: "none",
            truth_intended_planet_indices: &[],
            truth: PlanetMap { planets: vec![] },
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 250.0, 0, 8),
            target_tick: 50,
        },
        Scenario {
            name: "one_planet_noncontact_flyby",
            family: "single_planet_flyby",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[0],
            truth: one(),
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "flat_diagonal",
            family: "flat_baseline",
            defect_class: "none",
            truth_intended_planet_indices: &[],
            truth: PlanetMap { planets: vec![] },
            observations: vec![(launch(200.0, 400.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(250.0, 700.0, 35, 8),
            target_tick: 60,
        },
        Scenario {
            name: "one_planet_distant_flyby",
            family: "single_planet_flyby",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[0],
            truth: one(),
            observations: vec![(launch(200.0, 200.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_closer_flyby",
            family: "single_planet_flyby",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[0],
            truth: one(),
            observations: vec![(launch(200.0, 380.0, 0, 8), Termination::BattlefieldMargin)],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_reverse_flyby",
            family: "single_planet_flyby",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[0],
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
            family: "single_contact",
            defect_class: "underconstrained_planet_geometry",
            truth_intended_planet_indices: &[0],
            truth: PlanetMap {
                planets: vec![planet(600.0, 500.0, 20.0)],
            },
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 400.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "large_planet_single_contact",
            family: "single_contact",
            defect_class: "underconstrained_planet_geometry",
            truth_intended_planet_indices: &[0],
            truth: PlanetMap {
                planets: vec![planet(600.0, 500.0, 40.0)],
            },
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 300.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_single_contact",
            family: "single_contact",
            defect_class: "underconstrained_planet_geometry",
            truth_intended_planet_indices: &[0],
            truth: one(),
            observations: vec![(launch(200.0, 500.0, 0, 8), Termination::PlanetCollision(0))],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 70,
        },
        Scenario {
            name: "one_planet_established",
            family: "established_map",
            defect_class: "map_recovery_baseline",
            truth_intended_planet_indices: &[],
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
            family: "partial_map",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[1],
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
            family: "multi_planet_flyby",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[0, 1],
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
            family: "established_map",
            defect_class: "map_recovery_baseline",
            truth_intended_planet_indices: &[],
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
            family: "established_map",
            defect_class: "multi_planet_swerve",
            truth_intended_planet_indices: &[],
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
            name: "three_planets_partial_swerve",
            family: "partial_map",
            defect_class: "unseen_planet",
            truth_intended_planet_indices: &[2],
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
            ],
            trial: launch(200.0, 350.0, 0, 8),
            target_tick: 120,
        },
        Scenario {
            name: "four_planets_four_contact_map",
            family: "established_map",
            defect_class: "multi_planet_map_recovery",
            truth_intended_planet_indices: &[],
            truth: PlanetMap {
                planets: vec![
                    planet(500.0, 400.0, 25.0),
                    planet(850.0, 600.0, 35.0),
                    planet(1200.0, 350.0, 30.0),
                    planet(1500.0, 700.0, 40.0),
                ],
            },
            observations: vec![
                (launch(200.0, 400.0, 0, 8), Termination::PlanetCollision(0)),
                (
                    launch(850.0, 100.0, -90, 8),
                    Termination::PlanetCollision(1),
                ),
                (
                    launch(1200.0, 100.0, -90, 8),
                    Termination::PlanetCollision(2),
                ),
                (
                    launch(1500.0, 100.0, -90, 8),
                    Termination::PlanetCollision(3),
                ),
            ],
            trial: launch(200.0, 700.0, 0, 8),
            target_tick: 50,
        },
        Scenario {
            name: "two_planets_established",
            family: "established_map",
            defect_class: "multi_planet_map_recovery",
            truth_intended_planet_indices: &[],
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
    .into_iter()
    .chain(std::iter::once(w7::scenario()))
    .collect()
}

fn scenario_capture_exists(path: &Path, scenario: &str, revision: &str) -> Result<bool, String> {
    if !path.exists() {
        return Ok(false);
    }
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|error| error.to_string())?;
    let has_captures = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captures')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    if !has_captures {
        return Ok(false);
    }
    let mut statement = connection
        .prepare("SELECT ended_at_unix_ms, metadata_json FROM captures")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, Option<i64>>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (ended_at, text) = row.map_err(|e| e.to_string())?;
        if ended_at.is_none() {
            continue;
        }
        let metadata: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if metadata["host"] == "synthetic-scenario"
            && metadata["client_name"] == scenario
            && metadata["build"]["git_revision"] == revision
            && metadata["build"]["source_changes"] == false
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn discard_incomplete_scenario_captures(
    path: &Path,
    scenario: &str,
    revision: &str,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut connection = rusqlite::Connection::open(path).map_err(|error| error.to_string())?;
    let has_captures = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='captures')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| error.to_string())?;
    if !has_captures {
        return Ok(());
    }
    let mut statement = connection
        .prepare("SELECT id, ended_at_unix_ms, metadata_json FROM captures")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut incomplete = Vec::new();
    for row in rows {
        let (id, ended_at, text) = row.map_err(|error| error.to_string())?;
        let metadata: serde_json::Value =
            serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if ended_at.is_none()
            && metadata["host"] == "synthetic-scenario"
            && metadata["client_name"] == scenario
            && metadata["build"]["git_revision"] == revision
            && metadata["build"]["source_changes"] == false
        {
            incomplete.push(id);
        }
    }
    drop(statement);
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    for id in incomplete {
        transaction
            .execute("DELETE FROM events WHERE capture_id=?1", [id])
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM diagnostics WHERE capture_id=?1", [id])
            .map_err(|error| error.to_string())?;
        transaction
            .execute("DELETE FROM captures WHERE id=?1", [id])
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db_path = std::path::PathBuf::from("learning-scenarios.sqlite3");
    let mut record_captures = true;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db-path" => db_path = args.next().ok_or("--db-path requires a value")?.into(),
            "--no-captures" => record_captures = false,
            "--help" | "-h" => {
                println!(
                    "Usage: learning_scenarios [--db-path PATH] [--no-captures]\nRuns deterministic learning scenarios and, once per scenario per clean git revision, stores an inspectable SQLite capture."
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let revision = option_env!("EINSTEINBOT_GIT_REVISION");
    let source_clean = option_env!("EINSTEINBOT_GIT_DIRTY") == Some("false");
    if record_captures && (revision.is_none() || !source_clean) {
        eprintln!(
            "Not writing scenario captures without a known clean git revision; commit the version first or use --no-captures."
        );
        record_captures = false;
    }
    let revision = revision.unwrap_or_default();
    for scenario in scenarios() {
        let capture = if record_captures {
            discard_incomplete_scenario_captures(&db_path, scenario.name, revision)?;
            if scenario_capture_exists(&db_path, scenario.name, revision)? {
                println!(
                    "scenario_capture={} revision={} already exists; reusing",
                    scenario.name, revision
                );
                None
            } else {
                Some(ScenarioCapture::create(&db_path, &scenario)?)
            }
        } else {
            None
        };
        run(scenario, capture);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn training_and_witness_paths_are_physically_valid() {
        for scenario in scenarios() {
            for (launch, expected) in scenario.observations.iter().copied() {
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
    fn scenario_metadata_labels_only_explicit_expected_defects() {
        let scenarios = scenarios();
        for scenario in &scenarios {
            let metadata = scenario.capture_metadata();
            assert_eq!(metadata.name, scenario.name);
            assert!(!metadata.family.is_empty());
            assert!(!metadata.defect_class.is_empty());
            assert_eq!(metadata.seed, (scenario.family == "W7").then_some(w7::SEED));
            assert!(
                metadata.difficulty.is_none(),
                "no objective difficulty ranking was recorded"
            );
            assert!(
                scenario
                    .truth_intended_planet_indices
                    .iter()
                    .all(|index| *index < scenario.truth.planets.len())
            );

            let decision = scenario.diagnostic_metadata("partial");
            assert_eq!(decision["name"], metadata.name);
            assert!(decision.get("family").is_none());
            assert!(decision.get("params").is_none());
            assert!(decision.get("seed").is_none());
            assert!(decision.get("difficulty").is_none());
            assert_eq!(decision["stage"], "partial");
            assert_eq!(
                decision["intended"],
                serde_json::json!(scenario.truth_intended_planet_indices)
            );
            assert_eq!(
                metadata.params["truth_planet_count"],
                scenario.truth.planets.len()
            );
        }

        let find = |name| {
            scenarios
                .iter()
                .find(|scenario| scenario.name == name)
                .unwrap()
        };
        assert_eq!(
            find("one_planet_noncontact_flyby").truth_intended_planet_indices,
            &[0]
        );
        assert_eq!(
            find("two_planets_one_known").truth_intended_planet_indices,
            &[1]
        );
        assert_eq!(
            find("two_planets_flybys_only").truth_intended_planet_indices,
            &[0, 1]
        );
        assert_eq!(
            find("three_planets_partial_swerve").truth_intended_planet_indices,
            &[2]
        );
        assert!(
            find("three_planets_established_swerve")
                .truth_intended_planet_indices
                .is_empty()
        );
    }

    #[test]
    fn w7_fixture_is_seeded_scale_24_and_records_a_learning_curve() {
        let first = w7::scenario();
        let second = w7::scenario();
        assert_eq!(first.name, second.name);
        assert_eq!(
            first.capture_metadata().params,
            second.capture_metadata().params
        );
        assert_eq!(first.observations.len(), 8);
        assert_eq!(first.observations.len(), second.observations.len());
        assert!(first.observations.iter().all(|(launch, _)| {
            simulate_shot(
                &first.truth,
                &shot(*launch),
                WorldParameters::default(),
                2000,
            )
            .unwrap()
            .points
            .len()
                >= 40
        }));
        assert_eq!(first.truth.planets.len(), 24);
        assert!(
            first
                .truth
                .planets
                .iter()
                .all(|planet| (20.0..40.0).contains(&planet.radius))
        );
        let metadata = first.capture_metadata();
        assert_eq!(metadata.family, "W7");
        assert_eq!(metadata.seed, Some(w7::SEED));
        assert_eq!(
            metadata.params["training_path_count"],
            first.observations.len()
        );
        assert_eq!(
            metadata.params["training_paths"].as_array().unwrap().len(),
            first.observations.len()
        );
        assert!(first.target_tick > 0);
    }

    #[test]
    fn scenario_captures_are_deduplicated_by_name_and_revision() {
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-scenario-index-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE captures(id INTEGER PRIMARY KEY, ended_at_unix_ms INTEGER, metadata_json TEXT NOT NULL);
                 CREATE TABLE events(capture_id INTEGER);
                 CREATE TABLE diagnostics(capture_id INTEGER);",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO captures(ended_at_unix_ms, metadata_json) VALUES (100, ?1)",
                [serde_json::json!({
                    "host": "synthetic-scenario",
                    "client_name": "flat_direct",
                    "build": {"git_revision": "rev-a", "source_changes": false}
                })
                .to_string()],
            )
            .unwrap();
        assert!(scenario_capture_exists(&path, "flat_direct", "rev-a").unwrap());
        connection
            .execute(
                "INSERT INTO captures(ended_at_unix_ms, metadata_json) VALUES (NULL, ?1)",
                [serde_json::json!({
                    "host": "synthetic-scenario",
                    "client_name": "flat_direct",
                    "build": {"git_revision": "rev-a", "source_changes": false}
                })
                .to_string()],
            )
            .unwrap();
        let incomplete_id = connection.last_insert_rowid();
        connection
            .execute(
                "INSERT INTO events(capture_id) VALUES (?1)",
                [incomplete_id],
            )
            .unwrap();
        discard_incomplete_scenario_captures(&path, "flat_direct", "rev-a").unwrap();
        let remaining: i64 = connection
            .query_row("SELECT COUNT(*) FROM captures", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
        assert!(!scenario_capture_exists(&path, "flat_direct", "rev-b").unwrap());
        assert!(!scenario_capture_exists(&path, "one_planet", "rev-a").unwrap());
        drop(connection);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn flat_training_and_target_are_handled_without_inventing_a_planet() {
        let scenario = scenarios().remove(0);
        let mut estimator = Estimator::default();
        let mut state = BotState::default();
        state.observe(&ServerEvent::OwnId { player_id: 1 });
        for (stage, (launch, expected)) in scenario.observations.iter().copied().enumerate() {
            observe_launch(
                &mut estimator,
                &mut state,
                &scenario.truth,
                launch,
                expected,
                None,
                &scenario,
                stage + 1,
            );
        }
        assert!(estimator.belief().unwrap().map.planets.is_empty());
        let start = scenario.trial;
        state.set_player_position(
            1,
            Point {
                x: start.x,
                y: start.y,
            },
        );
        state.set_player_position(2, Point { x: 600.0, y: 250.0 });
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
