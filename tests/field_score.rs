use einsteinbot::estimator::{SeedStrategy, seed_planet_from_residuals_with_strategy};
use einsteinbot::field_score::{FieldModel, score_field};
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, Point};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;
use std::sync::atomic::AtomicBool;

fn world() -> WorldParameters {
    WorldParameters {
        battlefield_width: 40.0,
        battlefield_height: 40.0,
        ..WorldParameters::default()
    }
}
fn path() -> ShotObservation {
    ShotObservation {
        player_id: 1,
        angle_degrees: 0.0,
        velocity: 8.0,
        points: vec![
            Point { x: 1.0, y: 1.0 },
            Point { x: 39.0, y: 1.0 },
            Point { x: 39.0, y: 1.0 },
        ],
    }
}
fn planet(x: f64, y: f64) -> Planet {
    Planet {
        x,
        y,
        radius: 5.0,
        mass: 100.0,
    }
}
#[test]
fn score_weights_each_explored_grid_cell_once_and_keeps_unexplored_separate() {
    let truth = PlanetMap { planets: vec![] };
    let model = PlanetMap {
        planets: vec![planet(100.0, 100.0)],
    };
    let belief = MapBelief::new(model.clone(), None);
    let report = score_field(
        &truth,
        &belief,
        &[path(), path()],
        world(),
        FieldModel::Physical,
    )
    .unwrap();
    let explored = report.explored.unwrap();
    let unexplored = report.unexplored.unwrap();
    assert_eq!(
        (
            explored.samples,
            unexplored.samples,
            report.whole.unwrap().samples
        ),
        (1, 3, 4)
    );
    let expected = model
        .acceleration(einsteinbot::map::Vec2::new(30.0, 10.0))
        .unwrap();
    assert!((explored.vector_rmse.unwrap() - expected.norm_squared().sqrt()).abs() < 1e-12);
}
#[test]
fn singular_predictions_are_reported_instead_of_excluded() {
    let truth = PlanetMap { planets: vec![] };
    let belief = MapBelief::new(
        PlanetMap {
            planets: vec![planet(30.0, 10.0)],
        },
        None,
    );
    let report = score_field(&truth, &belief, &[path()], world(), FieldModel::Physical).unwrap();
    let explored = report.explored.unwrap();
    assert_eq!(explored.samples, 1);
    assert_eq!(explored.invalid, 1);
    assert!(explored.vector_rmse.is_none());
    let whole = report.whole.unwrap();
    assert_eq!(
        (whole.samples, whole.finite_samples, whole.invalid),
        (4, 3, 1)
    );
    assert!(whole.vector_rmse.is_none());
    assert!(whole.finite_only_vector_rmse.is_some());
    assert_eq!(report.unexplored.unwrap().invalid, 0);
}

#[test]
fn finite_extreme_field_errors_do_not_overflow_the_rmse() {
    let truth = PlanetMap { planets: vec![] };
    let belief = MapBelief::new(
        PlanetMap {
            planets: vec![Planet {
                x: 100.0,
                y: 100.0,
                radius: 5.0,
                mass: 1e200,
            }],
        },
        None,
    );
    let report = score_field(&truth, &belief, &[path()], world(), FieldModel::Physical).unwrap();
    let whole = report.whole.unwrap();
    assert_eq!(whole.invalid, 0);
    assert!(whole.vector_rmse.unwrap().is_finite());
}

#[test]
fn truth_field_scores_zero_and_truth_interiors_are_excluded_for_all_models() {
    let truth = PlanetMap {
        planets: vec![planet(10.0, 10.0)],
    };
    let belief = MapBelief::new(truth.clone(), None);
    let report = score_field(&truth, &belief, &[path()], world(), FieldModel::Physical).unwrap();
    assert_eq!(report.excluded_truth_interior, 1);
    assert_eq!(report.whole.unwrap().samples, 3);
    assert_eq!(report.explored.unwrap().vector_rmse, Some(0.0));
}

#[test]
fn validated_field_uses_the_source_only_with_its_matching_residual() {
    let truth = PlanetMap {
        planets: vec![planet(100.0, 100.0)],
    };
    let mut belief = MapBelief::new(PlanetMap { planets: vec![] }, None);
    belief.field_hypothesis = Some(einsteinbot::field_hypothesis::FieldHypothesis {
        x: 100.0,
        y: 100.0,
        mass: 100.0,
        support: vec![],
        support_paths: vec![],
        baseline_continuation_rmse: 1.0,
        continuation_rmse: 0.0,
        training_paths: 1,
        validation_paths: 1,
        relative_improvement: 1.0,
    });
    assert!(score_field(&truth, &belief, &[path()], world(), FieldModel::Validated).is_none());
    let source_only = score_field(
        &truth,
        &belief,
        &[path()],
        world(),
        FieldModel::ValidatedSourceOnly,
    )
    .unwrap();
    assert!(source_only.whole.unwrap().vector_rmse.unwrap() < 1e-15);
    belief.hypothesis_local_field = Some(Default::default());
    let matched = score_field(&truth, &belief, &[path()], world(), FieldModel::Validated).unwrap();
    // Source and physical-map acceleration use different f64 arithmetic orders.
    assert!(matched.whole.unwrap().vector_rmse.unwrap() < 1e-15);
    let without_source =
        score_field(&truth, &belief, &[path()], world(), FieldModel::NoSource).unwrap();
    assert!(without_source.whole.unwrap().vector_rmse.unwrap() > 0.0);
}

#[test]
fn provisional_source_rebuilds_a_nonempty_residual_without_double_counting() {
    let truth = PlanetMap {
        planets: vec![planet(100.0, 100.0)],
    };
    let flat = PlanetMap { planets: vec![] };
    let mut observed = ShotObservation {
        player_id: 1,
        angle_degrees: 0.0,
        velocity: 8.0,
        points: vec![Point { x: 1.0, y: 10.0 }],
    };
    observed.points =
        einsteinbot::simulation::simulate_sampled_path(&truth, &observed, world(), 6).unwrap();
    let mut belief = MapBelief::new(flat.clone(), None);
    belief.local_field =
        einsteinbot::local_field::LocalField::from_observations(&flat, &[observed.clone()]);
    let source = einsteinbot::field_hypothesis::FieldHypothesis {
        x: 100.0,
        y: 100.0,
        mass: 100.0,
        support: vec![],
        support_paths: vec![],
        baseline_continuation_rmse: 1.0,
        continuation_rmse: 0.0,
        training_paths: 1,
        validation_paths: 0,
        relative_improvement: 1.0,
    };
    belief.provisional_field_hypothesis =
        Some(einsteinbot::field_hypothesis::ProvisionalFieldHypothesis {
            source,
            approximate_covariance: None,
            covariance_unavailable_reason: None,
        });
    let score = score_field(
        &truth,
        &belief,
        &[observed],
        world(),
        FieldModel::Provisional,
    )
    .unwrap();
    let explored = score.explored.unwrap();
    assert!(explored.residual_supported.unwrap() > 0);
    assert!(
        explored.vector_rmse.unwrap() < 1e-4,
        "adding the original residual would count the source twice"
    );
}
#[test]
fn empty_exploration_and_unavailable_hypothesis_are_not_zero_scores() {
    let truth = PlanetMap { planets: vec![] };
    let belief = MapBelief::new(truth.clone(), None);
    let report = score_field(&truth, &belief, &[], world(), FieldModel::NoSource).unwrap();
    assert!(report.explored.is_none());
    assert_eq!((report.explored_cells, report.unexplored_cells), (0, 4));
    let whole = report.whole.unwrap();
    assert_eq!(whole.vector_rmse, Some(0.0));
    assert_eq!(whole.residual_supported, Some(0));
    assert!(score_field(&truth, &belief, &[], world(), FieldModel::Validated).is_none());
}

#[test]
fn seeding_variants_are_independent_and_grid_is_the_default_after_reset() {
    let truth = PlanetMap {
        planets: vec![Planet {
            x: 600.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        }],
    };
    let mut shot = ShotObservation {
        player_id: 1,
        angle_degrees: 0.0,
        velocity: 8.0,
        points: vec![Point { x: 200.0, y: 400.0 }],
    };
    shot.points =
        einsteinbot::simulation::simulate_shot(&truth, &shot, WorldParameters::default(), 2000)
            .unwrap()
            .points;
    let flat = PlanetMap { planets: vec![] };
    let grid = seed_planet_from_residuals_with_strategy(
        &flat,
        &[shot.clone()],
        WorldParameters::default(),
        30.0,
        50.0,
        SeedStrategy::Grid,
        &AtomicBool::new(false),
    )
    .unwrap();
    let bearing = seed_planet_from_residuals_with_strategy(
        &flat,
        &[shot],
        WorldParameters::default(),
        30.0,
        50.0,
        SeedStrategy::Bearing,
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!((grid.x - 600.0).hypot(grid.y - 500.0) > 5.0);
    assert!((bearing.x - 600.0).hypot(bearing.y - 500.0) < 5.0);
    let mut variant =
        einsteinbot::online_map::OnlineEstimator::with_seed_strategy(SeedStrategy::Bearing);
    variant.reset();
    assert_eq!(variant.seed_strategy(), SeedStrategy::Bearing);
    variant.observe(
        &einsteinbot::protocol::ServerEvent::Planets { planets: vec![] },
        &AtomicBool::new(false),
    );
    assert_eq!(variant.seed_strategy(), SeedStrategy::Bearing);
    assert_eq!(
        einsteinbot::online_map::OnlineEstimator::default().seed_strategy(),
        SeedStrategy::Grid
    );
}

#[test]
fn online_variants_consume_the_same_flybys_but_publish_their_own_source() {
    use einsteinbot::protocol::ServerEvent;
    let truth = PlanetMap {
        planets: vec![Planet {
            x: 600.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        }],
    };
    let mut grid = einsteinbot::online_map::OnlineEstimator::with_seed_strategy(SeedStrategy::Grid);
    let mut bearing =
        einsteinbot::online_map::OnlineEstimator::with_seed_strategy(SeedStrategy::Bearing);
    let cancel = AtomicBool::new(false);
    for y in [400.0, 350.0] {
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y }],
        };
        let points =
            einsteinbot::simulation::simulate_shot(&truth, &shot, WorldParameters::default(), 2000)
                .unwrap()
                .points;
        let position = ServerEvent::PlayerPosition {
            player_id: 1,
            x: 200.0,
            y,
        };
        let completed = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 8.0,
            points,
        };
        for estimator in [&mut grid, &mut bearing] {
            estimator.observe(&position, &cancel);
            estimator.observe(&completed, &cancel);
        }
    }
    let grid_source = grid
        .belief()
        .unwrap()
        .field_hypothesis
        .as_ref()
        .expect("grid supports source");
    let bearing_source = bearing
        .belief()
        .unwrap()
        .field_hypothesis
        .as_ref()
        .expect("bearing supports source");
    assert!((grid_source.x - bearing_source.x).hypot(grid_source.y - bearing_source.y) > 1.0);
}
