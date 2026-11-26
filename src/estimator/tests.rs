use super::*;
use crate::protocol::Planet;
use crate::simulation::{Termination, simulate_shot};

fn planet(x: f64, y: f64, radius: f64) -> Planet {
    Planet {
        x,
        y,
        radius,
        mass: radius.powi(3) / 10.0,
    }
}

fn world_of(planets: Vec<Planet>) -> PlanetMap {
    PlanetMap { planets }
}

/// A completed shot as the server would report it, simulated in `truth`.
fn shot_in(truth: &PlanetMap, start: (f32, f32), angle: f64, velocity: f64) -> ServerEvent {
    let shot = ShotObservation {
        player_id: 1,
        angle_degrees: angle,
        velocity,
        points: vec![Point {
            x: start.0,
            y: start.1,
        }],
    };
    let path = simulate_shot(truth, &shot, WorldParameters::default(), 600).unwrap();
    ServerEvent::ShotFin {
        player_id: 1,
        angle,
        velocity,
        points: path.points,
    }
}

fn position(player_id: u32, x: f32, y: f32) -> ServerEvent {
    ServerEvent::PlayerPosition { player_id, x, y }
}

fn shot_of(event: &ServerEvent) -> ShotObservation {
    ShotObservation::try_from(event).unwrap()
}

fn rmse(map: &PlanetMap, event: &ServerEvent) -> f64 {
    shot_position_rmse(map, &shot_of(event), WorldParameters::default())
        .unwrap()
        .unwrap()
}

/// An estimator that knows one far-away player, so terminal points are classified as contacts.
fn estimator() -> (Estimator, AtomicBool) {
    let mut estimator = Estimator::default();
    let cancelled = AtomicBool::new(false);
    estimator.observe(&position(9, 1800.0, 1000.0), &cancelled);
    (estimator, cancelled)
}

fn contact_truth() -> PlanetMap {
    world_of(vec![planet(500.0, 500.0, 30.0)])
}

/// Head-on path ending on the planet's surface.
fn contact_event() -> ServerEvent {
    let event = shot_in(&contact_truth(), (350.0, 500.0), 0.0, 4.0);
    let path = shot_of(&event);
    let truth_end = simulate_shot(&contact_truth(), &path, WorldParameters::default(), 600)
        .unwrap()
        .termination;
    assert_eq!(truth_end, Termination::PlanetCollision(0));
    event
}

// Acceptance 1: flat world.
#[test]
fn flat_world_keeps_no_planet() {
    let (mut estimator, cancelled) = estimator();
    let flat = world_of(vec![]);
    let mut belief = None;
    for (start, angle, velocity) in [
        ((300.0, 300.0), 20.0, 10.0),
        ((900.0, 800.0), 160.0, 12.0),
        ((1500.0, 500.0), 200.0, 9.0),
        ((600.0, 100.0), -30.0, 11.0),
    ] {
        belief = estimator.observe(&shot_in(&flat, start, angle, velocity), &cancelled);
        let belief = belief.as_ref().unwrap();
        assert!(belief.map.planets.is_empty(), "{:?}", belief.map);
    }
    let belief = belief.unwrap();
    assert!(belief.map_weight.unwrap() > 0.5);
    assert_eq!(
        belief.covariance_unavailable_reason,
        Some(CovarianceUnavailableReason::NoPlanets)
    );
    assert!(belief.parameter_covariance.is_none());
    assert_eq!(belief.unmatched_contacts, 0);
}

#[test]
fn flat_world_with_small_position_noise_keeps_no_planet() {
    let (mut estimator, cancelled) = estimator();
    let flat = world_of(vec![]);
    for (index, (start, angle, velocity)) in [
        ((300.0, 300.0), 20.0, 10.0),
        ((900.0, 800.0), 160.0, 12.0),
        ((1500.0, 500.0), 200.0, 9.0),
    ]
    .into_iter()
    .enumerate()
    {
        let ServerEvent::ShotFin {
            player_id,
            angle,
            velocity,
            mut points,
        } = shot_in(&flat, start, angle, velocity)
        else {
            unreachable!()
        };
        // Deterministic +-0.1 unit wobble, well inside the noise floor.
        for (i, point) in points.iter_mut().enumerate() {
            let wobble = 0.1 * (((i + 7 * index) % 5) as f32 - 2.0) / 2.0;
            point.x += wobble;
            point.y -= wobble;
        }
        let belief = estimator
            .observe(
                &ServerEvent::ShotFin {
                    player_id,
                    angle,
                    velocity,
                    points,
                },
                &cancelled,
            )
            .unwrap();
        assert!(belief.map.planets.is_empty());
    }
}

// Acceptance 2: a planet learned from earlier shots predicts a later one better than flat.
#[test]
fn one_planet_world_beats_flat_on_a_held_out_shot() {
    let truth = world_of(vec![planet(900.0, 500.0, 30.0)]);
    let (mut estimator, cancelled) = estimator();
    let training = [
        shot_in(&truth, (500.0, 380.0), 0.0, 10.0),
        shot_in(&truth, (900.0, 150.0), 90.0, 8.0),
    ];
    let held_out = shot_in(&truth, (400.0, 700.0), 15.0, 11.0);
    let mut belief = None;
    for event in &training {
        belief = estimator.observe(event, &cancelled);
    }
    let belief = belief.unwrap();
    assert_eq!(belief.map.planets.len(), 1, "{:?}", belief);
    let learned = rmse(&belief.map, &held_out);
    let flat = rmse(&world_of(vec![]), &held_out);
    assert!(learned < flat / 2.0, "learned={learned} flat={flat}");
}

fn problem<'a>(shots: &'a [Shot], cancelled: &'a AtomicBool, use_contacts: bool) -> Problem<'a> {
    Problem {
        shots,
        world: WorldParameters::default(),
        cancelled,
        use_contacts,
    }
}

fn shot_with_contact(event: &ServerEvent, players: &[Point]) -> Shot {
    let obs = shot_of(event);
    let contact = contacts::classify(&obs.points, players, WorldParameters::default());
    Shot { obs, contact }
}

fn center_error(theta: [f64; 3]) -> f64 {
    (theta[0] - 500.0).hypot(theta[1] - 500.0)
}

// A hit's trajectory alone identifies the planet; the contact only tightens it.
#[test]
fn trajectory_alone_recovers_a_hit_and_the_contact_tightens_it() {
    let cancelled = AtomicBool::new(false);
    let event = contact_event();
    let shots = [shot_with_contact(
        &event,
        &[Point {
            x: 1800.0,
            y: 1000.0,
        }],
    )];
    assert!(shots[0].contact.is_some());
    // Start well off the truth (28 units, 5 radius units).
    let start = [520.0, 480.0, 35.0];
    let (alone, _) = problem(&shots, &cancelled, false).refine(start).unwrap();
    let (joint, _) = problem(&shots, &cancelled, true).refine(alone).unwrap();
    assert!(
        center_error(alone) < 0.5 && (alone[2] - 30.0).abs() < 0.5,
        "{alone:?}"
    );
    assert!(
        center_error(joint) < 1.0 && (joint[2] - 30.0).abs() < 0.5,
        "{joint:?}"
    );
    // The contact refines rather than overrides: it moves the fit by far less than the
    // trajectory-only uncertainty, and tightens centre (along the approach) and radius.
    assert!((alone[0] - joint[0]).hypot(alone[1] - joint[1]) < 1.0);
    let sd = |use_contacts, theta| {
        let (_, covariance) = problem(&shots, &cancelled, use_contacts)
            .laplace(theta, 0.0)
            .unwrap();
        let c = covariance.expect("a hit is identified");
        (c[(0, 0)].sqrt(), c[(1, 1)].sqrt(), c[(2, 2)].sqrt())
    };
    let (alone_sd, joint_sd) = (sd(false, alone), sd(true, joint));
    assert!(
        alone_sd.0 < 15.0 && alone_sd.1 < 5.0 && alone_sd.2 < 3.0,
        "{alone_sd:?}"
    );
    assert!(
        joint_sd.0 < alone_sd.0 / 3.0 && joint_sd.2 < alone_sd.2 / 3.0,
        "{joint_sd:?}"
    );
}

#[test]
fn a_hit_is_recovered_tightly_from_one_path_through_the_estimator() {
    let (mut estimator, cancelled) = estimator();
    let event = contact_event();
    let belief = estimator
        .observe(&event, &cancelled)
        .expect("planet from a hit");
    assert_eq!(belief.map.planets.len(), 1);
    let planet = &belief.map.planets[0];
    assert_eq!(planet.mass, planet.radius.powi(3) / 10.0);
    assert!(
        (planet.x - 500.0).hypot(planet.y - 500.0) < 1.0,
        "{planet:?}"
    );
    assert!((planet.radius - 30.0).abs() < 0.5, "{planet:?}");
    assert_eq!(belief.unmatched_contacts, 0);
    assert_eq!(belief.contact_points.len(), 1);
    let covariance = belief.parameter_covariance.as_ref().expect("identified");
    assert!(covariance[(0, 0)].sqrt() < 1.0 && covariance[(1, 1)].sqrt() < 3.0);
    let learned = rmse(&belief.map, &event);
    let flat = rmse(&world_of(vec![]), &event);
    assert!(learned < flat / 10.0, "learned={learned} flat={flat}");
    // Duplicates add nothing; a map reset forgets everything.
    assert!(estimator.observe(&event, &cancelled).is_none());
    assert_eq!(estimator.shots.len(), 1);
    estimator.reset();
    assert!(estimator.belief().is_none() && estimator.shots.is_empty());
}

fn hit_from(start: (f32, f32), angle: f64) -> ServerEvent {
    shot_in(&contact_truth(), start, angle, 4.0)
}

#[test]
fn contacts_from_several_directions_give_one_sharper_planet() {
    let (mut estimator, cancelled) = estimator();
    let mut errors = Vec::new();
    for (start, angle) in [
        ((350.0, 500.0), 0.0),
        ((500.0, 650.0), 90.0),
        ((650.0, 500.0), 180.0),
    ] {
        let belief = estimator
            .observe(&hit_from(start, angle), &cancelled)
            .unwrap();
        // Never several planets for one body, however many overlapping contacts arrive.
        assert_eq!(belief.map.planets.len(), 1, "{:?}", belief.map);
        let planet = &belief.map.planets[0];
        errors.push((planet.x - 500.0).hypot(planet.y - 500.0));
        assert!((planet.radius - 30.0).abs() < 1.0, "{planet:?}");
    }
    assert!(errors.iter().all(|e| *e < 2.0), "{errors:?}");
    let belief = estimator.belief().unwrap();
    assert_eq!(belief.contact_points.len(), 3);
    assert_eq!(belief.unmatched_contacts, 0);
}

#[test]
fn a_curved_flyby_does_not_contradict_the_planet() {
    let (mut estimator, cancelled) = estimator();
    estimator.observe(&contact_event(), &cancelled).unwrap();
    // Passes 100 units above the centre and bends toward it; the chord of its samples stays
    // outside the disk but the path is curved.
    let flyby = shot_in(&contact_truth(), (300.0, 400.0), 0.0, 6.0);
    let belief = estimator.observe(&flyby, &cancelled).unwrap();
    assert_eq!(belief.map.planets.len(), 1);
    let planet = &belief.map.planets[0];
    assert!(
        (planet.x - 500.0).hypot(planet.y - 500.0) < 1.0,
        "{planet:?}"
    );
}

// Later free space through the supposed planet must not leave a planet whose disk contains
// observed regular samples.
#[test]
fn later_free_space_never_leaves_a_planet_containing_observed_samples() {
    let (mut estimator, cancelled) = estimator();
    estimator.observe(&contact_event(), &cancelled).unwrap();
    let through = shot_in(&world_of(vec![]), (300.0, 500.0), 0.0, 6.0);
    let belief = estimator.observe(&through, &cancelled).unwrap();
    for shot in &estimator.shots {
        let regular = &shot.obs.points[..shot.obs.points.len() - 1];
        for planet in &belief.map.planets {
            assert!(
                regular.iter().all(|p| {
                    (f64::from(p.x) - planet.x).hypot(f64::from(p.y) - planet.y) >= planet.radius
                }),
                "{planet:?} contains an observed sample"
            );
        }
    }
}

#[test]
fn constant_acceleration_contact_is_kept_as_a_contact_but_creates_no_planet() {
    let (mut estimator, cancelled) = estimator();
    let points = (0..10)
        .map(|i| Point {
            x: 500.0 + i as f32,
            y: 500.0,
        })
        .collect::<Vec<_>>();
    let event = ServerEvent::ShotFin {
        player_id: 1,
        angle: 0.0,
        velocity: 1.0,
        points: points.clone(),
    };
    let belief = estimator.observe(&event, &cancelled).unwrap();
    assert!(belief.map.planets.is_empty());
    assert_eq!(belief.contact_points, vec![points.last().unwrap().clone()]);
    assert_eq!(belief.unmatched_contacts, 1);
}

#[test]
fn a_contact_outside_the_battlefield_cannot_create_a_planet_there() {
    let (mut estimator, cancelled) = estimator();
    // Flat flight out of the battlefield, stopping inside the world margin at x = -300.
    let leaving = prefix(&shot_in(&world_of(vec![]), (100.0, 500.0), 180.0, 10.0), 41);
    let belief = estimator.observe(&leaving, &cancelled).unwrap();
    let world = WorldParameters::default();
    assert!(belief.map.planets.iter().all(|p| {
        p.x >= p.radius
            && p.y >= p.radius
            && p.x <= world.battlefield_width - p.radius
            && p.y <= world.battlefield_height - p.radius
    }));
    assert!(belief.map.planets.is_empty());
    assert_eq!(belief.contact_points.len(), 1);
}

#[test]
fn contacts_are_classified_with_the_players_known_at_each_shot() {
    let mut estimator = Estimator::default();
    let cancelled = AtomicBool::new(false);
    let flat = world_of(vec![]);
    let first = prefix(&shot_in(&flat, (300.0, 300.0), 0.0, 10.0), 41);
    let second = prefix(&shot_in(&flat, (300.0, 500.0), 0.0, 10.0), 41);
    let end = |event: &ServerEvent| shot_of(event).points.last().unwrap().clone();
    let events = vec![
        position(1, 100.0, 100.0),
        position(2, end(&first).x, end(&first).y),
        first.clone(),
        position(2, 1500.0, 1000.0),
        second.clone(),
    ];
    let belief = estimator.observe_batch(&events, &cancelled).unwrap();
    assert_eq!(estimator.shots.len(), 2);
    // The first ended on a known player; only the second is a possible contact.
    assert_eq!(belief.contact_points, vec![end(&second)]);
}

#[test]
fn player_positions_survive_a_map_reset_for_contact_classification() {
    // The server sends a player's new position just before that player's Planets event.
    let mut estimator = Estimator::default();
    let cancelled = AtomicBool::new(false);
    let ended_on_player = prefix(&shot_in(&world_of(vec![]), (300.0, 300.0), 0.0, 10.0), 41);
    let end = shot_of(&ended_on_player).points.last().unwrap().clone();
    estimator.observe(&position(2, end.x, end.y), &cancelled);
    estimator.reset();
    estimator.observe(&position(1, 100.0, 100.0), &cancelled);
    let belief = estimator.observe(&ended_on_player, &cancelled).unwrap();
    assert!(belief.contact_points.is_empty());
    assert!(belief.map.planets.is_empty());
}

#[test]
fn unknown_players_and_the_world_margin_create_no_contact() {
    let flat = world_of(vec![]);
    let cancelled = AtomicBool::new(false);
    let near_end = prefix(&shot_in(&flat, (300.0, 300.0), 0.0, 10.0), 41);
    // No player known: a player hit cannot be ruled out.
    let mut unknown = Estimator::default();
    assert!(
        unknown
            .observe(&near_end, &cancelled)
            .unwrap()
            .contact_points
            .is_empty()
    );
    // Ended at the world margin.
    let (mut estimator, cancelled) = estimator();
    let margin = shot_in(&flat, (300.0, 300.0), 0.0, 10.0);
    let belief = estimator.observe(&margin, &cancelled).unwrap();
    assert!(
        belief.contact_points.is_empty(),
        "{:?}",
        belief.contact_points
    );
}

#[test]
fn a_planets_event_forgets_earlier_evidence_inside_a_batch() {
    let (mut estimator, cancelled) = estimator();
    estimator.observe(&contact_event(), &cancelled).unwrap();
    assert_eq!(estimator.shots.len(), 1);
    let flat_shot = shot_in(&world_of(vec![]), (200.0, 400.0), 0.0, 8.0);
    estimator.observe_batch(
        &[ServerEvent::Planets { planets: vec![] }, flat_shot],
        &cancelled,
    );
    assert_eq!(estimator.shots.len(), 1);
    assert!(estimator.belief().unwrap().map.planets.is_empty());
    assert!(estimator.warm_starts.is_empty());
}

#[test]
fn cancellation_returns_no_belief() {
    let (mut estimator, _) = estimator();
    let cancelled = AtomicBool::new(true);
    assert!(estimator.observe(&contact_event(), &cancelled).is_none());
}

#[test]
fn contacts_beyond_the_gate_exert_no_pull_and_are_reported_as_mismatch() {
    // A contact far from every fitted surface exerts no pull and is counted, not hidden. (The
    // one-planet model cannot explain two planets; that limit is why the count is reported.)
    assert_eq!(fit::contact_residual(50.0, 0.5), 0.0);
    assert_eq!(fit::contact_residual(-50.0, 0.5), 0.0);
    assert!(fit::contact_residual(2.0, 0.5).abs() > 1.0);
    let (mut estimator, cancelled) = estimator();
    estimator.observe(&contact_event(), &cancelled).unwrap();
    let elsewhere = {
        let truth = world_of(vec![planet(1200.0, 700.0, 30.0)]);
        shot_in(&truth, (1050.0, 700.0), 0.0, 4.0)
    };
    let belief = estimator.observe(&elsewhere, &cancelled).unwrap();
    assert_eq!(belief.map.planets.len(), 1);
    assert_eq!(belief.contact_points.len(), 2);
    assert!(
        belief.unmatched_contacts >= 1,
        "{}",
        belief.unmatched_contacts
    );
}
/// A shot cut after `samples` ticks, as if it had ended in free flight (no known players, so no
/// contact is inferred from its terminal point).
fn prefix(event: &ServerEvent, samples: usize) -> ServerEvent {
    let ServerEvent::ShotFin {
        player_id,
        angle,
        velocity,
        points,
    } = event
    else {
        unreachable!()
    };
    ServerEvent::ShotFin {
        player_id: *player_id,
        angle: *angle,
        velocity: *velocity,
        points: points[..samples].to_vec(),
    }
}

// Acceptance 3: a short, weak flyby is explained by planets at very different distances (a
// heavier planet farther away deflects alike). The belief keeps both until a shot that sees the
// difference arrives.
#[test]
fn ambiguous_early_shots_keep_both_modes_until_a_later_shot_shifts_weight() {
    let truth = world_of(vec![planet(400.0, 922.0, 30.0)]);
    let (mut estimator, cancelled) = (Estimator::default(), AtomicBool::new(false));
    let early = prefix(&shot_in(&truth, (370.0, 100.0), 0.0, 1.5), 41);
    let before = estimator.observe(&early, &cancelled).unwrap();

    let mut modes = vec![(before.map.clone(), before.map_weight.unwrap())];
    modes.extend(
        before
            .alternatives
            .iter()
            .map(|m| (m.map.clone(), m.weight)),
    );
    assert_eq!(modes.len(), 2, "{modes:?}");
    assert!(
        modes
            .iter()
            .all(|(map, weight)| map.planets.len() == 1 && *weight > 0.1)
    );
    assert!(
        modes[0].1 < 0.9,
        "weight collapsed onto one mode: {modes:?}"
    );
    let (first, second) = (&modes[0].0.planets[0], &modes[1].0.planets[0]);
    assert!(
        (first.x - second.x).hypot(first.y - second.y) > 50.0,
        "{modes:?}"
    );
    // Both really do explain the early shot to within the noise floor.
    for (map, _) in &modes {
        assert!(rmse(map, &early) < 0.5, "{map:?}: {}", rmse(map, &early));
    }
    let true_mode = |map: &PlanetMap| {
        let planet = &map.planets[0];
        (planet.x - 400.0).hypot(planet.y - 922.0) < 5.0
    };
    let weight_of_truth = |belief: &MapBelief| {
        std::iter::once((&belief.map, belief.map_weight.unwrap()))
            .chain(belief.alternatives.iter().map(|m| (&m.map, m.weight)))
            .find(|(map, _)| !map.planets.is_empty() && true_mode(map))
            .map_or(0.0, |(_, weight)| weight)
    };
    let before_truth = weight_of_truth(&before);

    // A path launched near the candidates' difference separates them.
    let later = prefix(&shot_in(&truth, (650.0, 850.0), 180.0, 3.0), 41);
    let after = estimator.observe(&later, &cancelled).unwrap();
    assert!(true_mode(&after.map));
    assert!(weight_of_truth(&after) > 0.99 && weight_of_truth(&after) > before_truth);
}

fn good_shot() -> ServerEvent {
    shot_in(&world_of(vec![]), (300.0, 300.0), 20.0, 10.0)
}

#[test]
fn non_finite_shots_are_ignored_without_panicking() {
    let (mut estimator, cancelled) = estimator();
    let ServerEvent::ShotFin {
        player_id,
        angle,
        velocity,
        mut points,
    } = good_shot()
    else {
        unreachable!()
    };
    points[3].x = f32::NAN;
    let bad = ServerEvent::ShotFin {
        player_id,
        angle,
        velocity,
        points,
    };
    assert!(estimator.observe(&bad, &cancelled).is_none());
    assert!(estimator.shots.is_empty());
    let belief = estimator.observe(&good_shot(), &cancelled).unwrap();
    assert!(belief.map.planets.is_empty());
    assert!(belief.model_position_rmse.is_some_and(f64::is_finite));
}

#[test]
fn a_cancelled_fit_is_retried_on_the_next_event() {
    let (mut estimator, _) = estimator();
    // A shot was ingested but its fit never published (cancelled or failed).
    let obs = shot_of(&good_shot());
    estimator.shots.push(Shot { obs, contact: None });
    estimator.stale = true;
    let cancelled = AtomicBool::new(true);
    assert!(
        estimator
            .observe(&position(9, 1.0, 1.0), &cancelled)
            .is_none()
    );
    assert!(estimator.stale);
    cancelled.store(false, Ordering::SeqCst);
    let belief = estimator.observe(&position(9, 2.0, 2.0), &cancelled);
    assert!(belief.is_some());
    assert!(!estimator.stale);
}

// A mode at the radius bound has no trustworthy Gaussian approximation: no covariance, never a
// tight-looking one. The same shots at an interior radius do identify the planet.
#[test]
fn a_mode_at_a_bound_reports_no_covariance() {
    let cancelled = AtomicBool::new(false);
    let shots_for = |truth: &PlanetMap| {
        [
            shot_in(truth, (500.0, 380.0), 0.0, 10.0),
            shot_in(truth, (900.0, 150.0), 90.0, 8.0),
        ]
        .map(|event| Shot {
            obs: shot_of(&event),
            contact: None,
        })
    };
    let covariance_at = |radius: f64| {
        let truth = world_of(vec![planet(900.0, 500.0, radius)]);
        let shots = shots_for(&truth);
        let problem = problem(&shots, &cancelled, false);
        problem.laplace([900.0, 500.0, radius], 0.0).unwrap().1
    };
    assert!(covariance_at(30.0).is_some());
    assert!(covariance_at(40.0).is_none());
    assert!(covariance_at(20.0).is_none());
}

/// Deterministic uniform values in `[0, 1)`.
fn uniform(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

// Measurement, not a gate: how often K=1 recovers a real planet (centre within 5 units, radius
// within 2) from contact-free paths. Run with
// `cargo test recovery_rate -- --ignored --nocapture`.
#[test]
#[ignore]
fn recovery_rate_of_one_planet_worlds() {
    let mut state = 12345u64;
    for (label, aimed, shot_count) in [
        ("aimed past the planet, 4 shots", true, 4),
        ("blind, 4 shots", false, 4),
        ("aimed past the planet, 1 shot", true, 1),
        ("blind, 1 shot", false, 1),
    ] {
        let (mut recovered, mut chosen, worlds) = (0, 0, 30);
        for _ in 0..worlds {
            let radius = 20.0 + 20.0 * uniform(&mut state);
            let center = (
                300.0 + 1400.0 * uniform(&mut state),
                200.0 + 600.0 * uniform(&mut state),
            );
            let truth = world_of(vec![planet(center.0, center.1, radius)]);
            let mut estimator = Estimator::default();
            let cancelled = AtomicBool::new(false);
            let mut belief = None;
            for _ in 0..shot_count {
                let start = (
                    100.0 + 1700.0 * uniform(&mut state),
                    100.0 + 800.0 * uniform(&mut state),
                );
                let target = if aimed {
                    // Pass 50-200 units beside the planet.
                    let side = 50.0 + 150.0 * uniform(&mut state);
                    let around = std::f64::consts::TAU * uniform(&mut state);
                    (
                        center.0 + side * around.cos(),
                        center.1 + side * around.sin(),
                    )
                } else {
                    (
                        100.0 + 1700.0 * uniform(&mut state),
                        100.0 + 800.0 * uniform(&mut state),
                    )
                };
                let angle = (-(target.1 - f64::from(start.1)))
                    .atan2(target.0 - f64::from(start.0))
                    .to_degrees();
                let velocity = 8.0 + 4.0 * uniform(&mut state);
                let event = shot_in(&truth, (start.0 as f32, start.1 as f32), angle, velocity);
                belief = estimator.observe(&event, &cancelled).or(belief);
            }
            if let Some(found) = belief.and_then(|b| b.map.planets.first().cloned()) {
                chosen += 1;
                if (found.x - center.0).hypot(found.y - center.1) < 5.0
                    && (found.radius - radius).abs() < 2.0
                {
                    recovered += 1;
                }
            }
        }
        println!(
            "recovery [{label}]: planet chosen {chosen}/{worlds}, recovered {recovered}/{worlds}"
        );
    }
}
