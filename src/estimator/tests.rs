use super::*;
use crate::protocol::Planet;
use crate::simulation::{Termination, simulate_shot};
use fit::Fit;
use std::time::Duration;

/// Replay: one event, then the search work it owes.
trait ObserveSettled {
    fn observe_settled(&mut self, event: &ServerEvent, cancelled: &AtomicBool)
    -> Option<MapBelief>;
}

impl ObserveSettled for Estimator {
    fn observe_settled(
        &mut self,
        event: &ServerEvent,
        cancelled: &AtomicBool,
    ) -> Option<MapBelief> {
        let local = self.observe(event, cancelled)?;
        Some(self.settle(cancelled).unwrap_or(local))
    }
}

pub(super) fn planet(x: f64, y: f64, radius: f64) -> Planet {
    Planet {
        x,
        y,
        radius,
        mass: radius.powi(3) / 10.0,
    }
}

pub(super) fn world_of(planets: Vec<Planet>) -> PlanetMap {
    PlanetMap { planets }
}

/// A completed shot as the server would report it, simulated in `truth`.
pub(super) fn shot_in(
    truth: &PlanetMap,
    start: (f32, f32),
    angle: f64,
    velocity: f64,
) -> ServerEvent {
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
    estimator.observe_settled(&position(9, 1800.0, 1000.0), &cancelled);
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
        belief = estimator.observe_settled(&shot_in(&flat, start, angle, velocity), &cancelled);
        let belief = belief.as_ref().unwrap();
        assert!(belief.map.planets.is_empty(), "{:?}", belief.map);
    }
    let belief = belief.unwrap();
    assert_eq!(
        belief.covariance_unavailable_reason,
        Some(CovarianceUnavailableReason::NoPlanets)
    );
    assert!(belief.parameter_covariance.is_none());
    assert_eq!(belief.unmatched_contacts, 0);
}

/// Deterministic roughly Gaussian values (sum of uniforms), standard deviation 1.
fn gaussian(state: &mut u64) -> f64 {
    ((0..12).map(|_| uniform(state)).sum::<f64>() - 6.0) as f64
}

/// A flat-world shot with independent Gaussian noise of standard deviation `sd` on every point.
fn noisy_flat_shot(state: &mut u64, sd: f64) -> ServerEvent {
    let start = (
        (200.0 + 1500.0 * uniform(state)) as f32,
        (150.0 + 700.0 * uniform(state)) as f32,
    );
    let ServerEvent::ShotFin {
        player_id,
        angle,
        velocity,
        mut points,
    } = shot_in(
        &world_of(vec![]),
        start,
        360.0 * uniform(state),
        6.0 + 8.0 * uniform(state),
    )
    else {
        unreachable!()
    };
    for point in points.iter_mut() {
        point.x += (sd * gaussian(state)) as f32;
        point.y += (sd * gaussian(state)) as f32;
    }
    ServerEvent::ShotFin {
        player_id,
        angle,
        velocity,
        points,
    }
}

// Acceptance 1: noise well above the f32 rounding of the data, but far below any real field
// effect, must not create a planet.
#[test]
fn flat_world_with_small_position_noise_keeps_no_planet() {
    let (mut estimator, cancelled) = estimator();
    let mut state = 4242u64;
    for _ in 0..4 {
        let event = noisy_flat_shot(&mut state, 1e-3);
        let belief = estimator.observe_settled(&event, &cancelled).unwrap();
        assert!(belief.map.planets.is_empty(), "{:?}", belief.map);
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
        belief = estimator.observe_settled(event, &cancelled);
    }
    let belief = belief.unwrap();
    assert_eq!(belief.map.planets.len(), 1, "{:?}", belief);
    let learned = rmse(&belief.map, &held_out);
    let flat = rmse(&world_of(vec![]), &held_out);
    assert!(learned < flat / 2.0, "learned={learned} flat={flat}");
}

pub(super) fn shot_with_contact(event: &ServerEvent, players: &[Point]) -> Shot {
    let obs = shot_of(event);
    let contact = contacts::classify(&obs.points, players, WorldParameters::default());
    Shot::new(obs, contact, WorldParameters::default())
}

fn center_error(theta: &[f64; 3]) -> f64 {
    (theta[0] - 500.0).hypot(theta[1] - 500.0)
}

// A hit's trajectory alone identifies the planet; the contact refines it without overriding it.
#[test]
fn trajectory_alone_recovers_a_hit_and_the_contact_refines_it() {
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
    let fit = |use_contacts| {
        Fit::new(
            &shots,
            WorldParameters::default(),
            &cancelled,
            use_contacts,
            Vec::new(),
        )
    };
    // Start well off the truth (28 units, 5 radius units).
    let start = [[520.0, 480.0, 35.0]];
    let (alone, _) = fit(false).refine(&start, &fit::Budget::UNLIMITED).unwrap();
    let (joint, _) = fit(true).refine(&alone, &fit::Budget::UNLIMITED).unwrap();
    assert!(
        center_error(&alone[0]) < 0.5 && (alone[0][2] - 30.0).abs() < 0.5,
        "{alone:?}"
    );
    assert!(
        center_error(&joint[0]) < 1.0 && (joint[0][2] - 30.0).abs() < 0.5,
        "{joint:?}"
    );
    // The contact moves the fit by far less than a unit, and its information only adds.
    assert!((alone[0][0] - joint[0][0]).hypot(alone[0][1] - joint[0][1]) < 1.0);
    let sd = |use_contacts, theta: &[[f64; 3]]| {
        let covariance = fit(use_contacts)
            .evidence(theta)
            .unwrap()
            .covariance
            .expect("a hit is identified");
        (0..3)
            .map(|i| covariance[(i, i)].sqrt())
            .collect::<Vec<_>>()
    };
    let (alone_sd, joint_sd) = (sd(false, &alone), sd(true, &joint));
    assert!(
        alone_sd.iter().all(|s| *s < 2.0),
        "trajectory alone is already tight: {alone_sd:?}"
    );
    assert!(
        joint_sd.iter().zip(&alone_sd).all(|(j, a)| *j <= *a * 1.01),
        "{joint_sd:?} vs {alone_sd:?}"
    );
}

/// After one path the model-error scale is the prequential estimate against the flat map, which
/// counts the planet's own field as error of a map with unseen planets (P13): the planet is found
/// near its true place, and the covariance is wider than the floor alone would give and covers
/// the error.
#[test]
fn a_hit_is_recovered_from_one_path_through_the_estimator() {
    let (mut estimator, cancelled) = estimator();
    let event = contact_event();
    let belief = estimator
        .observe_settled(&event, &cancelled)
        .expect("planet from a hit");
    assert_eq!(belief.map.planets.len(), 1);
    let planet = &belief.map.planets[0];
    assert!((planet.mass - planet.radius.powi(3) / 10.0).abs() < 1e-9);
    let error = (planet.x - 500.0).hypot(planet.y - 500.0);
    assert!(error < 2.0, "{planet:?}");
    assert!((planet.radius - 30.0).abs() < 1.0, "{planet:?}");
    assert_eq!(belief.unmatched_contacts, 0);
    assert_eq!(belief.contact_points.len(), 1);
    let covariance = belief.parameter_covariance.as_ref().expect("identified");
    let sd = covariance[(0, 0)].sqrt().max(covariance[(1, 1)].sqrt());
    assert!(sd < 10.0 && error < 3.0 * sd, "error {error} sd {sd}");
    let learned = rmse(&belief.map, &event);
    let flat = rmse(&world_of(vec![]), &event);
    assert!(learned < flat / 10.0, "learned={learned} flat={flat}");
    // Duplicates add nothing; a map reset forgets everything.
    assert!(estimator.observe_settled(&event, &cancelled).is_none());
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
            .observe_settled(&hit_from(start, angle), &cancelled)
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
    estimator
        .observe_settled(&contact_event(), &cancelled)
        .unwrap();
    // Passes 100 units above the centre and bends toward it; the chord of its samples stays
    // outside the disk but the path is curved.
    let flyby = shot_in(&contact_truth(), (300.0, 400.0), 0.0, 6.0);
    let belief = estimator.observe_settled(&flyby, &cancelled).unwrap();
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
    estimator
        .observe_settled(&contact_event(), &cancelled)
        .unwrap();
    let through = shot_in(&world_of(vec![]), (300.0, 500.0), 0.0, 6.0);
    let belief = estimator.observe_settled(&through, &cancelled).unwrap();
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
    let belief = estimator.observe_settled(&event, &cancelled).unwrap();
    assert!(belief.map.planets.is_empty());
    assert_eq!(belief.contact_points, vec![points.last().unwrap().clone()]);
    assert_eq!(belief.unmatched_contacts, 1);
}

#[test]
fn a_contact_outside_the_battlefield_cannot_create_a_planet_there() {
    let (mut estimator, cancelled) = estimator();
    // Flat flight out of the battlefield, stopping inside the world margin at x = -300.
    let leaving = prefix(&shot_in(&world_of(vec![]), (100.0, 500.0), 180.0, 10.0), 41);
    let belief = estimator.observe_settled(&leaving, &cancelled).unwrap();
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
    estimator.observe_settled(&position(2, end.x, end.y), &cancelled);
    estimator.reset();
    estimator.observe_settled(&position(1, 100.0, 100.0), &cancelled);
    let belief = estimator
        .observe_settled(&ended_on_player, &cancelled)
        .unwrap();
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
            .observe_settled(&near_end, &cancelled)
            .unwrap()
            .contact_points
            .is_empty()
    );
    // Ended at the world margin.
    let (mut estimator, cancelled) = estimator();
    let margin = shot_in(&flat, (300.0, 300.0), 0.0, 10.0);
    let belief = estimator.observe_settled(&margin, &cancelled).unwrap();
    assert!(
        belief.contact_points.is_empty(),
        "{:?}",
        belief.contact_points
    );
}

#[test]
fn a_planets_event_forgets_earlier_evidence_inside_a_batch() {
    let (mut estimator, cancelled) = estimator();
    estimator
        .observe_settled(&contact_event(), &cancelled)
        .unwrap();
    assert_eq!(estimator.shots.len(), 1);
    let flat_shot = shot_in(&world_of(vec![]), (200.0, 400.0), 0.0, 8.0);
    estimator.observe_batch(
        &[ServerEvent::Planets { planets: vec![] }, flat_shot],
        &cancelled,
    );
    assert_eq!(estimator.shots.len(), 1);
    assert!(estimator.belief().unwrap().map.planets.is_empty());
    assert!(estimator.search.theta.is_empty());
}

#[test]
fn cancellation_returns_no_belief() {
    let (mut estimator, _) = estimator();
    let cancelled = AtomicBool::new(true);
    assert!(
        estimator
            .observe_settled(&contact_event(), &cancelled)
            .is_none()
    );
}

#[test]
fn contacts_beyond_the_gate_exert_no_pull() {
    assert_eq!(fit::contact_residual(50.0, 0.5), 0.0);
    assert_eq!(fit::contact_residual(-50.0, 0.5), 0.0);
    assert!(fit::contact_residual(2.0, 0.5).abs() > 1.0);
}

// Two separate hits are two planets, and each contact is explained by its own planet's surface.
#[test]
fn separate_hits_give_separate_planets_with_no_unmatched_contact() {
    let truth = world_of(vec![
        planet(500.0, 500.0, 30.0),
        planet(1200.0, 700.0, 30.0),
    ]);
    let (mut estimator, cancelled) = estimator();
    estimator
        .observe_settled(&shot_in(&truth, (350.0, 500.0), 0.0, 4.0), &cancelled)
        .unwrap();
    let belief = estimator
        .observe_settled(&shot_in(&truth, (1050.0, 700.0), 0.0, 4.0), &cancelled)
        .unwrap();
    assert_eq!(belief.map.planets.len(), 2, "{:?}", belief.map);
    assert_eq!(belief.contact_points.len(), 2);
    assert_eq!(belief.unmatched_contacts, 0);
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
    assert!(estimator.observe_settled(&bad, &cancelled).is_none());
    assert!(estimator.shots.is_empty());
    let belief = estimator.observe_settled(&good_shot(), &cancelled).unwrap();
    assert!(belief.map.planets.is_empty());
    // Not computed live (P18): absent means unavailable.
    assert!(belief.model_position_rmse.is_none());
}

// A mode at the radius bound has no trustworthy Gaussian approximation: no covariance, never a
// tight-looking one. The same shots at an interior radius do identify the planet.
#[test]
fn a_mode_at_a_bound_reports_no_covariance() {
    let cancelled = AtomicBool::new(false);
    let covariance_at = |radius: f64| {
        let truth = world_of(vec![planet(900.0, 500.0, radius)]);
        let shots = [
            shot_in(&truth, (500.0, 380.0), 0.0, 10.0),
            shot_in(&truth, (900.0, 150.0), 90.0, 8.0),
        ]
        .map(|event| Shot::new(shot_of(&event), None, WorldParameters::default()));
        Fit::new(
            &shots,
            WorldParameters::default(),
            &cancelled,
            false,
            Vec::new(),
        )
        .evidence(&[[900.0, 500.0, radius]])
        .unwrap()
        .covariance
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
                belief = estimator.observe_settled(&event, &cancelled).or(belief);
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

// The optimizer leaves its last trial step as the parameters when the deadline interrupts it; the
// refit returns the best point it evaluated instead. This sweep is a smoke test: the interrupted
// trial is rarely worse than the *start*, so it does not by itself fail without the fix.
#[test]
fn a_deadline_never_leaves_a_refit_worse_than_its_start() {
    let cancelled = AtomicBool::new(false);
    let truth = world_of(vec![
        planet(500.0, 500.0, 30.0),
        planet(1200.0, 700.0, 30.0),
    ]);
    let shots: Vec<Shot> = [
        ((350.0, 500.0), 0.0),
        ((1050.0, 700.0), 0.0),
        ((900.0, 600.0), 150.0),
        ((300.0, 900.0), 20.0),
    ]
    .iter()
    .map(|(start, angle)| shot_with_contact(&shot_in(&truth, *start, *angle, 6.0), &[]))
    .collect();
    let start = [[520.0, 480.0, 35.0], [1180.0, 720.0, 26.0]];
    for micros in (0..12_000).step_by(600) {
        let fit = Fit::new(
            &shots,
            WorldParameters::default(),
            &cancelled,
            false,
            Vec::new(),
        )
        .with_deadline(Instant::now() + Duration::from_micros(micros));
        let (theta, chi2) = fit.refine(&start, &fit::Budget::UNLIMITED).unwrap();
        let before = fit.chi2(&start).unwrap();
        assert!(
            chi2 <= before * (1.0 + 1e-9),
            "{micros} us: {chi2} after, {before} before ({theta:?})"
        );
    }
}

fn three_planet_events() -> Vec<ServerEvent> {
    let truth = world_of(vec![
        planet(500.0, 500.0, 30.0),
        planet(1200.0, 700.0, 30.0),
        planet(900.0, 200.0, 25.0),
    ]);
    [
        ((350.0, 500.0), 0.0),
        ((1050.0, 700.0), 0.0),
        ((900.0, 400.0), 90.0),
    ]
    .iter()
    .map(|(start, angle)| shot_in(&truth, *start, *angle, 6.0))
    .collect()
}

// The planet cap and the slice limits are reported, and a belief is published either way.
#[test]
fn planet_cap_and_slice_limits_are_reported() {
    let truth = world_of(vec![
        planet(500.0, 500.0, 30.0),
        planet(1200.0, 700.0, 30.0),
    ]);
    let events = [
        shot_in(&truth, (350.0, 500.0), 0.0, 4.0),
        shot_in(&truth, (1050.0, 700.0), 0.0, 4.0),
    ];
    let (mut capped, cancelled) = estimator();
    capped.settings.max_planets = 1;
    let belief = events
        .iter()
        .filter_map(|event| capped.observe_settled(event, &cancelled))
        .last()
        .unwrap();
    assert_eq!(belief.map.planets.len(), 1);
    assert_eq!(belief.search_limit, Some(SearchLimit::PlanetCap));

    // Right after a path the slices are still owed; with no time per slice none gets anywhere.
    let (mut hurried, cancelled) = estimator();
    hurried.settings.slice_ceiling = Some(Duration::ZERO);
    let belief = hurried.observe(&events[0], &cancelled).unwrap();
    assert_eq!(belief.search_limit, Some(SearchLimit::WorkBudget));
    let belief = hurried.settle(&cancelled).unwrap();
    assert_eq!(belief.search_limit, Some(SearchLimit::TimeBudget));

    let (mut free, cancelled) = estimator();
    free.settings.slices_per_path = 50;
    let belief = free.observe_settled(&events[0], &cancelled).unwrap();
    assert_eq!(belief.search_limit, None);
    assert_eq!(belief.map.planets.len(), 1);
}

// The slice limit is a work count: the belief and the work done do not depend on the clock or on
// how many threads share the work.
#[test]
fn the_slice_limit_gives_the_same_belief_whatever_the_load() {
    let events = three_planet_events();
    let run = |threads: usize| {
        let (mut estimator, cancelled) = estimator();
        estimator.settings.slice_evaluations = 100;
        estimator.settings.slice_ceiling = None;
        estimator.settings.threads = threads;
        let mut work = 0;
        let mut belief = None;
        for event in &events {
            belief = estimator.observe_settled(event, &cancelled);
            work += estimator.last_report().unwrap().evaluations;
        }
        (belief.unwrap(), work)
    };
    let (first, work) = run(1);
    for threads in [1, 4] {
        let (again, again_work) = run(threads);
        assert_eq!(again.map.planets, first.map.planets, "{threads} threads");
        assert_eq!(again_work, work);
    }
}

// A slice exits promptly when cancelled, and a reset drops the pending work.
#[test]
fn a_cancelled_slice_stops_and_a_reset_drops_pending_work() {
    let (mut estimator, cancelled) = estimator();
    estimator
        .observe(&three_planet_events()[0], &cancelled)
        .unwrap();
    assert!(estimator.search_pending());
    let stop = AtomicBool::new(true);
    let started = Instant::now();
    estimator.settle(&stop);
    assert!(started.elapsed() < Duration::from_secs(5));
    estimator.reset();
    assert!(!estimator.search_pending() && estimator.belief().is_none());
}

// A part that exceeds its wall-clock ceiling is named in the report and fails loudly on demand;
// parts without a ceiling and runs within their ceilings report nothing.
#[test]
fn an_exceeded_part_ceiling_is_reported_by_name() {
    let events = three_planet_events();
    let (mut generous, cancelled) = estimator();
    generous.settings.max_planets = 2;
    generous
        .settings
        .ceilings
        .0
        .insert(Part::WindowBuild, Duration::from_secs(60));
    generous.observe_batch(&events, &cancelled).unwrap();
    generous.settle(&cancelled);
    let report = generous.last_report().unwrap();
    assert!(report.violations.is_empty());
    report.assert_within_ceilings();
    assert!(report.part(Part::WindowBuild).unwrap().calls == 3);
    for part in [Part::JacobianEval, Part::LmSolve, Part::BirthJoint] {
        assert!(report.part(part).is_some_and(|p| p.calls > 0), "{part:?}");
    }

    let (mut hurried, cancelled) = estimator();
    hurried
        .settings
        .ceilings
        .0
        .insert(Part::Evidence, Duration::from_nanos(1));
    hurried.observe_batch(&events, &cancelled).unwrap();
    hurried.settle(&cancelled);
    let report = hurried.last_report().unwrap();
    assert_eq!(
        report.violations.iter().map(|v| v.part).collect::<Vec<_>>(),
        vec![Part::Evidence]
    );
    let message = std::panic::catch_unwind(|| report.assert_within_ceilings())
        .unwrap_err()
        .downcast::<String>()
        .unwrap();
    assert!(message.contains("evidence"), "{message}");
}

// The analytic Jacobian matches finite differences of the residual (window rows, interior-sample
// penalties and contacts), and splitting an evaluation over threads changes no bit.
#[test]
fn the_jacobian_matches_finite_differences_and_threads_change_nothing() {
    let cancelled = AtomicBool::new(false);
    let truth = world_of(vec![
        planet(500.0, 500.0, 30.0),
        planet(1200.0, 700.0, 28.0),
        planet(900.0, 250.0, 25.0),
    ]);
    let shots: Vec<Shot> = [
        ((350.0, 500.0), 0.0),
        ((1050.0, 700.0), 0.0),
        ((900.0, 450.0), 90.0),
    ]
    .iter()
    .map(|(start, angle)| {
        shot_with_contact(
            &shot_in(&truth, *start, *angle, 6.0),
            &[Point { x: 1.0, y: 1.0 }],
        )
    })
    .collect();
    let fit = Fit::new(&shots, WorldParameters::default(), &cancelled, true, vec![]);
    let theta = [
        [505.0, 497.0, 30.5],
        [1195.0, 703.0, 27.0],
        [905.0, 255.0, 25.5],
    ];
    let (residual, jacobian) = fit.evaluate(&theta, true, 1).unwrap();
    let jacobian = jacobian.unwrap();
    for threads in [2, 3, 8] {
        let (r, j) = fit.evaluate(&theta, true, threads).unwrap();
        assert!(r == residual && j.unwrap() == jacobian, "{threads} threads");
    }
    let h = 1e-5;
    for column in 0..9 {
        let at = |sign: f64| {
            let mut moved = theta;
            moved[column / 3][column % 3] += sign * h;
            fit.evaluate(&moved, false, 1).unwrap().0
        };
        let numeric = (at(1.0) - at(-1.0)) / (2.0 * h);
        let scale = numeric.amax().max(1e-9);
        let error = (numeric - jacobian.column(column)).amax();
        assert!(error < 1e-3 * scale, "column {column}: {error} vs {scale}");
    }
}
