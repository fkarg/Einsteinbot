use einsteinbot::field_score::score_field;
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, Point};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;

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
    let report = score_field(&truth, &belief, &[path(), path()], world()).unwrap();
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
    let report = score_field(&truth, &belief, &[path()], world()).unwrap();
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
    let report = score_field(&truth, &belief, &[path()], world()).unwrap();
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
    let report = score_field(&truth, &belief, &[path()], world()).unwrap();
    assert_eq!(report.excluded_truth_interior, 1);
    assert_eq!(report.whole.unwrap().samples, 3);
    assert_eq!(report.explored.unwrap().vector_rmse, Some(0.0));
}

#[test]
fn empty_exploration_is_not_a_zero_score() {
    let truth = PlanetMap { planets: vec![] };
    let belief = MapBelief::new(truth.clone(), None);
    let report = score_field(&truth, &belief, &[], world()).unwrap();
    assert!(report.explored.is_none());
    assert_eq!((report.explored_cells, report.unexplored_cells), (0, 4));
    assert_eq!(report.whole.unwrap().vector_rmse, Some(0.0));
}
