//! Offline experiment: add a field-only source to an unchanged known physical map.
//! The known map is given as a fixture; truth is used solely to generate and score paths.
use einsteinbot::estimator::{seed_planet_from_residuals, shot_position_rmse};
use einsteinbot::event_store::EventLog;
use einsteinbot::field_hypothesis::FieldHypothesis;
use einsteinbot::local_field::LocalField;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::planet_fit::{ContactConstraint, fit_planet_map};
use einsteinbot::protocol::{Planet, Point};
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_sampled_positions, simulate_shot,
    simulate_targeting_shot,
};
use einsteinbot::strategy::{BotState, MapBelief, TargetingPolicy};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn planet(x: f64, y: f64, radius: f64) -> Planet {
    Planet {
        x,
        y,
        radius,
        mass: radius.powi(3) / 10.0,
    }
}
fn observed(map: &PlanetMap, y: f32) -> ShotObservation {
    let mut shot = ShotObservation {
        player_id: 1,
        angle_degrees: 0.0,
        velocity: 8.0,
        points: vec![Point { x: 200.0, y }],
    };
    let path = simulate_shot(map, &shot, WorldParameters::default(), 2000).unwrap();
    assert!(
        matches!(
            path.termination,
            Termination::BattlefieldMargin | Termination::PlanetCollision(_)
        ),
        "fixture must finish naturally"
    );
    shot.points = path.points;
    shot
}
fn continuation(map: &PlanetMap, shot: &ShotObservation, split: usize) -> f64 {
    let n = shot.points.len() - 1;
    let predicted = simulate_sampled_positions(map, shot, WorldParameters::default(), n).unwrap();
    (shot.points[split..n]
        .iter()
        .zip(&predicted[split..])
        .map(|(a, b)| (f64::from(a.x) - b.x).powi(2) + (f64::from(a.y) - b.y).powi(2))
        .sum::<f64>()
        / (2 * (n - split)) as f64)
        .sqrt()
}
fn score(map: &PlanetMap, shot: &ShotObservation, ticks: usize) -> Option<f64> {
    let mut s = shot.clone();
    s.points.truncate(ticks + 1);
    shot_position_rmse(map, &s, WorldParameters::default())
        .ok()
        .flatten()
}
/// Fit the contact-constrained known planet and a provisional physical-shaped source.
/// The latter's radius only regularizes optimization; callers evaluate it as gravity-only.
fn joint(
    known: &PlanetMap,
    seed: &Planet,
    contacts: &[ShotObservation],
    train: &ShotObservation,
    split: usize,
) -> Result<PlanetMap, String> {
    let radius = (10.0 * seed.mass).cbrt().clamp(20.0, 40.0);
    let mut map = known.clone();
    let world = WorldParameters::default();
    map.planets.push(Planet {
        x: seed.x.clamp(radius, world.battlefield_width - radius),
        y: seed.y.clamp(radius, world.battlefield_height - radius),
        radius,
        mass: radius.powi(3) / 10.0,
    });
    let mut early = train.clone();
    early.points.truncate(split + 1);
    let constraints = contacts
        .iter()
        .map(|contact| ContactConstraint {
            planet_index: 0,
            point: contact.points.last().unwrap().clone(),
        })
        .collect::<Vec<_>>();
    let mut observations = contacts.to_vec();
    observations.push(early);
    let fit = fit_planet_map(
        &map,
        &observations,
        &constraints,
        WorldParameters::default(),
        &AtomicBool::new(false),
    )?;
    let mut fitted = fit.map;
    // The fitted source is never an obstacle in predictions.
    fitted.planets.last_mut().unwrap().radius = 0.0;
    Ok(fitted)
}
fn run(name: &str, truth: &PlanetMap, known: &PlanetMap) {
    let started = Instant::now();
    let train = observed(truth, 400.0);
    let count = train.points.len() - 1;
    let split = count * 3 / 5;
    let mut early = train.clone();
    early.points.truncate(split + 1);
    let candidate =
        match seed_planet_from_residuals(known, &[early], WorldParameters::default(), 30.0, 50.0) {
            Ok(candidate) => candidate,
            Err(error) => {
                println!("scenario={name} no_candidate={error}");
                return;
            }
        };
    let mut alternative = known.clone();
    alternative.planets.push(Planet {
        radius: 0.0,
        ..candidate.clone()
    });
    let baseline = continuation(known, &train, split);
    let proposal = continuation(&alternative, &train, split);
    let contact = observed(truth, 500.0);
    let joint_map = joint(
        known,
        &candidate,
        std::slice::from_ref(&contact),
        &train,
        split,
    );
    match &joint_map {
        Ok(map) => println!(
            "  joint known_center=({:.3},{:.3}) known_radius={:.3} source=({:.3},{:.3}) mass={:.3} continuation={:.3}",
            map.planets[0].x,
            map.planets[0].y,
            map.planets[0].radius,
            map.planets[1].x,
            map.planets[1].y,
            map.planets[1].mass,
            continuation(map, &train, split)
        ),
        Err(error) => println!("  joint_failed={error}"),
    }
    println!(
        "scenario={name} candidate=({:.1},{:.1}) mass={:.6} continuation_baseline={baseline:.6} continuation_proposed={proposal:.6} accept_20pct={} elapsed_ms={}",
        candidate.x,
        candidate.y,
        candidate.mass,
        baseline > 0.01 && proposal < baseline * 0.8,
        started.elapsed().as_millis()
    );
    for y in [350.0, 250.0] {
        let heldout = observed(truth, y);
        println!(
            "  heldout_y={y} first70_baseline={:?} first70_proposed={:?} full_baseline={:?} full_proposed={:?}",
            score(known, &heldout, 70),
            score(&alternative, &heldout, 70),
            shot_position_rmse(known, &heldout, WorldParameters::default())
                .ok()
                .flatten(),
            shot_position_rmse(&alternative, &heldout, WorldParameters::default())
                .ok()
                .flatten()
        );
        if let Ok(map) = &joint_map {
            println!(
                "    joint_first70={:?} joint_full={:?}",
                score(map, &heldout, 70),
                shot_position_rmse(map, &heldout, WorldParameters::default())
                    .ok()
                    .flatten()
            );
        }
    }
    assert_eq!(
        &alternative.planets[..known.planets.len()],
        &known.planets[..],
        "known source must remain unchanged"
    );
}
/// Reproduce the two_planets_one_known scenario's first two contact shots before
/// attempting to explain the third, curved flyby. The true map is used only for
/// generating observations and final scoring.
fn learned_baseline_probe(truth: &PlanetMap) {
    let world = WorldParameters::default();
    let mut estimator = OnlineEstimator::default();
    let cancel = AtomicBool::new(false);
    let mut contacts = Vec::new();
    for (x, y, angle) in [(200.0, 500.0, 0.0), (600.0, 100.0, -90.0)] {
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: angle,
            velocity: 8.0,
            points: vec![Point { x, y }],
        };
        let path = simulate_shot(truth, &shot, world, 2000).unwrap();
        assert_eq!(path.termination, Termination::PlanetCollision(0));
        shot.points = path.points;
        estimator.observe(&ServerEvent::PlayerPosition { player_id: 1, x, y }, &cancel);
        estimator.observe(
            &ServerEvent::ShotFin {
                player_id: 1,
                angle,
                velocity: 8.0,
                points: shot.points.clone(),
            },
            &cancel,
        );
        contacts.push(shot);
    }
    let known = estimator.belief().unwrap().map.clone();
    assert_eq!(known.planets.len(), 1);
    let train = observed(truth, 400.0);
    let split = (train.points.len() - 1) * 3 / 5;
    let mut early = train.clone();
    early.points.truncate(split + 1);
    let started = Instant::now();
    let seed = match seed_planet_from_residuals(&known, &[early], world, 30.0, 50.0) {
        Ok(value) => value,
        Err(error) => {
            println!("learned_a no_seed={error}");
            return;
        }
    };
    let map = match joint(&known, &seed, &contacts, &train, split) {
        Ok(value) => value,
        Err(error) => {
            println!("learned_a joint_failed={error}");
            return;
        }
    };
    let seed_and_joint_fit_ms = started.elapsed().as_millis();
    let base_tail = continuation(&known, &train, split);
    let fit_tail = continuation(&map, &train, split);
    let heldout = observed(truth, 350.0);
    println!(
        "learned_a initial=({:.3},{:.3},r{:.3}) seed=({:.1},{:.1},m{:.2}) joint_a=({:.3},{:.3},r{:.3}) source=({:.3},{:.3},m{:.2}) tail_baseline={base_tail:.3} tail_joint={fit_tail:.3} accept_20pct={} witness_110_baseline={:?} witness_110_joint={:?}",
        known.planets[0].x,
        known.planets[0].y,
        known.planets[0].radius,
        seed.x,
        seed.y,
        seed.mass,
        map.planets[0].x,
        map.planets[0].y,
        map.planets[0].radius,
        map.planets[1].x,
        map.planets[1].y,
        map.planets[1].mass,
        base_tail > 0.01 && fit_tail < base_tail * 0.8,
        score(&known, &heldout, 110),
        score(&map, &heldout, 110)
    );
    // Reproduce the scenario's policy state and verify each chosen action in truth.
    let mut state = BotState::default();
    state.observe(&ServerEvent::OwnId { player_id: 1 });
    let mut observations = contacts.clone();
    observations.push(train.clone());
    for shot in &observations {
        let start = &shot.points[0];
        state.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: start.x,
            y: start.y,
        });
        state.observe(&ServerEvent::ShotFin {
            player_id: 1,
            angle: shot.angle_degrees,
            velocity: shot.velocity,
            points: shot.points.clone(),
        });
    }
    let target = heldout.points[110].clone();
    let start = heldout.points[0].clone();
    let players = [
        (1, Vec2::new(f64::from(start.x), f64::from(start.y))),
        (2, Vec2::new(f64::from(target.x), f64::from(target.y))),
    ];
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 1,
        x: start.x,
        y: start.y,
    });
    state.observe(&ServerEvent::PlayerPosition {
        player_id: 2,
        x: target.x,
        y: target.y,
    });
    let mut baseline_belief = MapBelief::new(known.clone(), None);
    baseline_belief.local_field = LocalField::from_observations(&known, &observations);
    let mut physical_joint = map.clone();
    physical_joint.planets.pop();
    let source = map.planets.last().unwrap();
    let mut joint_belief = MapBelief::new(physical_joint.clone(), None);
    joint_belief.local_field = LocalField::from_observations(&physical_joint, &observations);
    joint_belief.hypothesis_local_field = Some(LocalField::from_observations(&map, &observations));
    joint_belief.field_hypothesis = Some(FieldHypothesis {
        x: source.x,
        y: source.y,
        mass: source.mass,
        support: train.points[..train.points.len() - 1].to_vec(),
        baseline_continuation_rmse: base_tail,
        continuation_rmse: fit_tail,
    });
    println!(
        "  seed_and_joint_fit_ms={seed_and_joint_fit_ms} contacts={} fake_source_radius={}",
        contacts.len(),
        source.radius
    );
    // The standalone policy currently has no switch for hypothesis-only search. A
    // zero-radius point source has the same non-singular acceleration as the hypothesis;
    // validate the selected shot separately with the true field-only simulation below.
    let mut forced_belief = MapBelief::new(map.clone(), None);
    forced_belief.local_field = LocalField::from_observations(&map, &observations);
    for (name, belief) in [
        ("baseline", baseline_belief),
        ("joint", joint_belief.clone()),
        ("joint_only", forced_belief),
    ] {
        state.set_map_belief(Some(belief));
        let before = Instant::now();
        let Some(choice) = TargetingPolicy::default().choose(&state, 20.0) else {
            println!("  {name} no_choice");
            continue;
        };
        let search_ms = before.elapsed().as_millis();
        let candidate = ShotObservation {
            player_id: 1,
            angle_degrees: f64::from(choice.angle_degrees),
            velocity: f64::from(choice.velocity),
            points: vec![start.clone()],
        };
        let actual =
            simulate_targeting_shot(truth, &candidate, world, 2000, &players, 4.0).unwrap();
        let closest = actual
            .points
            .iter()
            .map(|p| f64::from(p.x - target.x).hypot(f64::from(p.y - target.y)))
            .fold(f64::INFINITY, f64::min);
        println!(
            "  {name} selected={choice:?} actual_termination={:?} closest_sample={closest:.3} search_ms={search_ms}",
            actual.termination
        );
        if name == "joint_only" {
            let predicted = einsteinbot::simulation::simulate_targeting_shot_with_hypothesis(
                &physical_joint,
                joint_belief.hypothesis_local_field.as_ref().unwrap(),
                joint_belief.field_hypothesis.as_ref().unwrap(),
                &candidate,
                world,
                180,
                &players,
                4.0,
            )
            .unwrap();
            println!("    forced_only_field_forecast={:?}", predicted.termination);
        }
    }
    // Exhaustive 1-degree diagnostic: evaluate each legal action only against the joint
    // forecast first. Truth is used afterward to score the forecast-hit set; it does not
    // influence which action ranks first under the policy's speed/impact-margin rule.
    let before = Instant::now();
    let mut predicted_hits = Vec::new();
    for velocity in 2..=20 {
        for angle in -180..180 {
            let candidate = ShotObservation {
                player_id: 1,
                angle_degrees: f64::from(angle),
                velocity: f64::from(velocity),
                points: vec![start.clone()],
            };
            if state.is_known_shot(&start, velocity, angle) {
                continue;
            }
            let Ok(path) = einsteinbot::simulation::simulate_targeting_shot_with_hypothesis(
                &physical_joint,
                joint_belief.hypothesis_local_field.as_ref().unwrap(),
                joint_belief.field_hypothesis.as_ref().unwrap(),
                &candidate,
                world,
                180,
                &players,
                4.0,
            ) else {
                continue;
            };
            if path.termination != Termination::PlayerCollision(2) {
                continue;
            }
            let end = &path.points[path.points.len() - 1];
            let previous = &path.points[path.points.len() - 2];
            let dx = f64::from(end.x - previous.x);
            let dy = f64::from(end.y - previous.y);
            let length = dx.hypot(dy);
            let margin = if length > 0.0 {
                (f64::from(target.x - end.x) * dy - f64::from(target.y - end.y) * dx).abs() / length
            } else {
                f64::INFINITY
            };
            predicted_hits.push((velocity, angle, margin));
        }
    }
    predicted_hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.total_cmp(&b.2)));
    let mut actual_hits = Vec::new();
    for &(velocity, angle, _) in &predicted_hits {
        let candidate = ShotObservation {
            player_id: 1,
            angle_degrees: f64::from(angle),
            velocity: f64::from(velocity),
            points: vec![start.clone()],
        };
        let path = simulate_targeting_shot(truth, &candidate, world, 2000, &players, 4.0).unwrap();
        if path.termination == Termination::PlayerCollision(2) {
            actual_hits.push((velocity, angle));
        }
    }
    println!(
        "  joint_only_dense_scan predicted_hit_commands={} forecast_ranked_first={:?} actual_hits_among_predicted={:?} elapsed_ms={}",
        predicted_hits.len(),
        predicted_hits.first(),
        actual_hits,
        before.elapsed().as_millis()
    );
}
fn capture() {
    let Ok(events) = EventLog::read_capture("einsteinbot.sqlite3", 29) else {
        println!("capture29 unavailable");
        return;
    };
    let mut estimator = OnlineEstimator::default();
    let cancelled = AtomicBool::new(false);
    let mut known: Option<(PlanetMap, ShotObservation)> = None;
    let mut learned: Option<(i64, PlanetMap, PlanetMap, Option<PlanetMap>)> = None;
    let mut pending: Vec<(i64, u32, f64, f64)> = Vec::new();
    let mut scored = 0;
    for row in events {
        let RecordedEvent::Inbound(event) = row.event else {
            continue;
        };
        match &event {
            ServerEvent::Planets { .. } => {
                estimator.reset();
                known = None;
                learned = None;
                pending.clear();
            }
            ServerEvent::PlayerPosition { .. } | ServerEvent::PlayerLeave { .. } => {
                estimator.observe(&event, &cancelled);
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => pending.push((row.sequence, *player_id, *angle, *velocity)),
            ServerEvent::ShotFin { .. } => {
                let shot = ShotObservation::try_from(&event).unwrap();
                let launch = pending
                    .iter()
                    .position(|(_, id, a, v)| {
                        (*id, *a, *v) == (shot.player_id, shot.angle_degrees, shot.velocity)
                    })
                    .map(|i| pending.remove(i).0);
                if let Some((cutoff, baseline, alternative, joint_map)) = &learned {
                    if launch.is_some_and(|n| n > *cutoff) && shot.points.len() > 4 {
                        println!(
                            "  capture_holdout_fin_seq={} first70_baseline={:?} first70_candidate={:?} full_baseline={:?} full_candidate={:?}",
                            row.sequence,
                            score(baseline, &shot, 70),
                            score(alternative, &shot, 70),
                            shot_position_rmse(baseline, &shot, WorldParameters::default())
                                .ok()
                                .flatten(),
                            shot_position_rmse(alternative, &shot, WorldParameters::default())
                                .ok()
                                .flatten()
                        );
                        if let Some(map) = joint_map {
                            println!(
                                "    capture_joint_first70={:?} capture_joint_full={:?}",
                                score(map, &shot, 70),
                                shot_position_rmse(map, &shot, WorldParameters::default())
                                    .ok()
                                    .flatten()
                            );
                        }
                        scored += 1;
                        if scored == 4 {
                            break;
                        }
                    }
                } else if let Some((baseline, contact)) = &known {
                    if shot.points.len() > 30 && launch.is_some() {
                        let split = (shot.points.len() - 1) * 3 / 5;
                        let mut early = shot.clone();
                        early.points.truncate(split + 1);
                        if let Ok(seed) = seed_planet_from_residuals(
                            baseline,
                            &[early],
                            WorldParameters::default(),
                            30.0,
                            50.0,
                        ) {
                            let mut alternative = baseline.clone();
                            alternative.planets.push(Planet {
                                radius: 0.0,
                                ..seed.clone()
                            });
                            let old = continuation(baseline, &shot, split);
                            let new = continuation(&alternative, &shot, split);
                            println!(
                                "capture29 train_fin_seq={} known_planets={} candidate=({:.1},{:.1}) mass={:.3} continuation_baseline={old:.3} continuation_candidate={new:.3} accept_20pct={}",
                                row.sequence,
                                baseline.planets.len(),
                                seed.x,
                                seed.y,
                                seed.mass,
                                old > 0.01 && new < old * 0.8
                            );
                            let joint_map = match joint(
                                baseline,
                                &seed,
                                std::slice::from_ref(contact),
                                &shot,
                                split,
                            ) {
                                Ok(map) => {
                                    println!(
                                        "  capture_joint known_center=({:.2},{:.2}) known_radius={:.2} source=({:.2},{:.2}) mass={:.2} continuation={:.2}",
                                        map.planets[0].x,
                                        map.planets[0].y,
                                        map.planets[0].radius,
                                        map.planets.last().unwrap().x,
                                        map.planets.last().unwrap().y,
                                        map.planets.last().unwrap().mass,
                                        continuation(&map, &shot, split)
                                    );
                                    Some(map)
                                }
                                Err(error) => {
                                    println!("  capture_joint_failed={error}");
                                    None
                                }
                            };
                            learned =
                                Some((row.sequence, baseline.clone(), alternative, joint_map));
                        }
                    }
                } else if let Some(belief) = estimator.observe(&event, &cancelled) {
                    if !belief.map.planets.is_empty() {
                        known = Some((belief.map, shot));
                    }
                }
            }
            _ => {}
        }
    }
    println!(
        "capture29 holdouts_scored={scored}; only shots launched after training completion; map frozen after first contact fit"
    );
}
fn main() {
    let a = planet(600.0, 500.0, 30.0);
    let b = planet(1000.0, 350.0, 35.0);
    let known = PlanetMap {
        planets: vec![a.clone()],
    };
    run("known_a_only", &known, &known);
    run(
        "known_a_plus_b",
        &PlanetMap {
            planets: vec![a, b],
        },
        &known,
    );
    learned_baseline_probe(&PlanetMap {
        planets: vec![planet(600.0, 500.0, 30.0), planet(1000.0, 350.0, 35.0)],
    });
    capture();
}
