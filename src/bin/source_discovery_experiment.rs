//! Offline comparison of the existing residual seeder followed by a physical planet fit.
//! Synthetic truth is used only to produce/score observations, never to initialize a fit.
use einsteinbot::estimator::{seed_planet_from_residuals, shot_position_rmse};
use einsteinbot::map::PlanetMap;
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::planet_fit::fit_planet_map;
use einsteinbot::protocol::{Planet, Point, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_sampled_positions, simulate_shot,
};
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
fn observed(
    map: &PlanetMap,
    x: f32,
    y: f32,
    angle: f64,
    velocity: f64,
    expected: Termination,
) -> ShotObservation {
    let mut shot = ShotObservation {
        player_id: 1,
        angle_degrees: angle,
        velocity,
        points: vec![Point { x, y }],
    };
    let path = simulate_shot(map, &shot, WorldParameters::default(), 2000).unwrap();
    assert_eq!(path.termination, expected, "fixture termination");
    shot.points = path.points;
    shot
}
fn learned_contact(truth: &PlanetMap) -> PlanetMap {
    let shot = observed(truth, 200., 500., 0., 8., Termination::PlanetCollision(0));
    let mut estimator = OnlineEstimator::default();
    let cancel = AtomicBool::new(false);
    estimator.observe(
        &ServerEvent::PlayerPosition {
            player_id: 1,
            x: 200.,
            y: 500.,
        },
        &cancel,
    );
    estimator
        .observe(
            &ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.,
                velocity: 8.,
                points: shot.points,
            },
            &cancel,
        )
        .unwrap()
        .map
}

/// Fit only early full-tick positions and evaluate the later positions without refitting.
fn continuation_rmse(map: &PlanetMap, observation: &ShotObservation, split: usize) -> f64 {
    let count = observation.points.len() - 1;
    let predicted =
        simulate_sampled_positions(map, observation, WorldParameters::default(), count).unwrap();
    let sum = observation.points[split..count]
        .iter()
        .zip(&predicted[split..])
        .map(|(p, q)| (f64::from(p.x) - q.x).powi(2) + (f64::from(p.y) - q.y).powi(2))
        .sum::<f64>();
    (sum / (2 * (count - split)) as f64).sqrt()
}

fn validate_continuation(baseline: &PlanetMap, full: &ShotObservation) {
    let count = full.points.len() - 1;
    let split = count * 3 / 5;
    let mut early = full.clone();
    // The fitter and reconstruction omit their last point as untimed. Include one extra
    // regular observation so indices up to split-1 are fit, and split onward is withheld.
    early.points.truncate(split + 1);
    let world = WorldParameters::default();
    let seed =
        match seed_planet_from_residuals(baseline, std::slice::from_ref(&early), world, 30., 50.) {
            Ok(seed) => seed,
            Err(error) => {
                println!("  continuation=no_proposal reason={error}");
                return;
            }
        };
    let radius = (10. * seed.mass).cbrt().clamp(20., 40.);
    let mut initial = baseline.clone();
    initial.planets.push(Planet {
        radius,
        mass: radius.powi(3) / 10.,
        ..seed
    });
    let fit = match fit_planet_map(&initial, &[early], &[], world, &AtomicBool::new(false)) {
        Ok(fit) => fit,
        Err(error) => {
            println!("  continuation=fit_failed reason={error}");
            return;
        }
    };
    let old = continuation_rmse(baseline, full, split);
    let proposed = continuation_rmse(&fit.map, full, split);
    println!(
        "  continuation withheld={} baseline_rmse={old:.6} proposed_rmse={proposed:.6} accept_if_better={}",
        count - split,
        proposed < old
    );
}

fn run(
    name: &str,
    truth: PlanetMap,
    baseline: PlanetMap,
    train: ShotObservation,
    holdout: ShotObservation,
) {
    let now = Instant::now();
    let world = WorldParameters::default();
    let baseline_error = shot_position_rmse(&baseline, &holdout, world)
        .unwrap()
        .unwrap();
    let mut short_holdout = holdout.clone();
    short_holdout.points.truncate(71);
    let baseline_short = shot_position_rmse(&baseline, &short_holdout, world)
        .unwrap()
        .unwrap();
    println!(
        "scenario={name} baseline_planets={} baseline_heldout_rmse={baseline_error:.4} baseline_70tick_rmse={baseline_short:.4}",
        baseline.planets.len()
    );
    validate_continuation(&baseline, &train);
    let seed = match seed_planet_from_residuals(
        &baseline,
        std::slice::from_ref(&train),
        world,
        30.,
        50.,
    ) {
        Ok(seed) => seed,
        Err(error) => {
            println!(
                "  no_proposal reason={error} elapsed_ms={}",
                now.elapsed().as_millis()
            );
            return;
        }
    };
    let radius = (10. * seed.mass).cbrt().clamp(20., 40.);
    let physical = Planet {
        radius,
        mass: radius.powi(3) / 10.,
        ..seed.clone()
    };
    println!(
        "  free_mass_seed x={:.1} y={:.1} mass={:.3} projected_radius={:.3}",
        seed.x, seed.y, seed.mass, radius
    );
    let mut initial = baseline.clone();
    initial.planets.push(physical);
    let fit = match fit_planet_map(
        &initial,
        std::slice::from_ref(&train),
        &[],
        world,
        &AtomicBool::new(false),
    ) {
        Ok(fit) => fit,
        Err(error) => {
            println!(
                "  fit_failed={error} elapsed_ms={}",
                now.elapsed().as_millis()
            );
            return;
        }
    };
    let proposal = fit.map.planets.last().unwrap();
    let heldout_error = shot_position_rmse(&fit.map, &holdout, world)
        .unwrap()
        .unwrap();
    let short_error = shot_position_rmse(&fit.map, &short_holdout, world)
        .unwrap()
        .unwrap();
    let nearest = truth
        .planets
        .iter()
        .map(|p| {
            (
                (p.x - proposal.x).hypot(p.y - proposal.y),
                (p.radius - proposal.radius).abs(),
            )
        })
        .min_by(|a, b| a.0.total_cmp(&b.0));
    println!(
        "  fitted x={:.3} y={:.3} radius={:.3} mass={:.3} nearest_truth={nearest:?} training_rmse={:.4} heldout_rmse={heldout_error:.4} heldout_70tick_rmse={short_error:.4} converged={} covariance={} elapsed_ms={}",
        proposal.x,
        proposal.y,
        proposal.radius,
        proposal.mass,
        fit.position_rmse,
        fit.converged,
        fit.covariance.is_some(),
        now.elapsed().as_millis()
    );
    println!(
        "  proposal_not_validated=true truth_planets={} inferred_planets={}",
        truth.planets.len(),
        fit.map.planets.len()
    );
}
fn main() {
    let flat = PlanetMap { planets: vec![] };
    let one = PlanetMap {
        planets: vec![planet(600., 500., 30.)],
    };
    let two = PlanetMap {
        planets: vec![planet(600., 500., 30.), planet(1000., 350., 35.)],
    };
    run(
        "flat_diagonal",
        flat.clone(),
        flat.clone(),
        observed(&flat, 200., 400., -7., 8., Termination::BattlefieldMargin),
        observed(&flat, 200., 350., 0., 8., Termination::BattlefieldMargin),
    );
    run(
        "one_close_flyby",
        one.clone(),
        flat.clone(),
        observed(&one, 200., 400., 0., 8., Termination::BattlefieldMargin),
        observed(&one, 200., 350., 0., 8., Termination::BattlefieldMargin),
    );
    run(
        "one_far_flyby",
        one.clone(),
        flat.clone(),
        observed(&one, 200., 200., 0., 8., Termination::BattlefieldMargin),
        observed(&one, 200., 350., 0., 8., Termination::BattlefieldMargin),
    );
    println!("scenario=flat_diagonal_sweep");
    for angle in (-169..=169).step_by(26) {
        let training = observed(
            &flat,
            900.,
            500.,
            f64::from(angle),
            6.,
            Termination::BattlefieldMargin,
        );
        print!("  angle={angle} ");
        validate_continuation(&flat, &training);
    }
    let known = learned_contact(&two);
    run(
        "known_a_unknown_b",
        two.clone(),
        known,
        observed(&two, 200., 400., 0., 8., Termination::BattlefieldMargin),
        observed(&two, 200., 350., 0., 8., Termination::BattlefieldMargin),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_and_flyby_fixtures_are_physically_realizable() {
        let flat = PlanetMap { planets: vec![] };
        let one = PlanetMap {
            planets: vec![planet(600., 500., 30.)],
        };
        let flat_shot = observed(&flat, 200., 400., -7., 8., Termination::BattlefieldMargin);
        let curved_shot = observed(&one, 200., 400., 0., 8., Termination::BattlefieldMargin);
        assert!(
            shot_position_rmse(&flat, &flat_shot, WorldParameters::default())
                .unwrap()
                .unwrap()
                < 0.001
        );
        assert!(
            shot_position_rmse(&flat, &curved_shot, WorldParameters::default())
                .unwrap()
                .unwrap()
                > 1.0
        );
    }
}
