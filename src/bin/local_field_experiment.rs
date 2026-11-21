//! Offline experiment, not a live targeting field. Acceleration estimates come only from
//! completed training trajectories; the true map is used solely to generate and score data.
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, reconstruct_acceleration,
    simulate_sampled_positions, simulate_shot,
};

// Deliberately simple, compact-support kernel. Its width is an experimental choice,
// not a confidence interval or a bound on the true field's spatial correlation.
const SUPPORT_RADIUS: f64 = 40.0;
const BANDWIDTH: f64 = 20.0;

struct LocalField {
    samples: Vec<(Vec2, Vec2)>,
}
impl LocalField {
    fn from_path(points: &[Point], base: &PlanetMap) -> Self {
        let samples = reconstruct_acceleration(points)
            .into_iter()
            .map(|(position, total)| {
                let modeled = base.acceleration(position).unwrap();
                (
                    position,
                    Vec2::new(total.x - modeled.x, total.y - modeled.y),
                )
            })
            .collect();
        Self { samples }
    }
    fn acceleration(&self, position: Vec2) -> Option<Vec2> {
        let mut sum = Vec2::default();
        let mut weights = 0.0;
        for (sample, acceleration) in &self.samples {
            let d2 = (sample.x - position.x).powi(2) + (sample.y - position.y).powi(2);
            if d2 > SUPPORT_RADIUS.powi(2) {
                continue;
            }
            let weight = (-d2 / (2.0 * BANDWIDTH.powi(2))).exp();
            sum.x += weight * acceleration.x;
            sum.y += weight * acceleration.y;
            weights += weight;
        }
        (weights > 0.0).then(|| Vec2::new(sum.x / weights, sum.y / weights))
    }
}

fn shot(y: f32) -> ShotObservation {
    ShotObservation {
        player_id: 1,
        angle_degrees: 0.0,
        velocity: 8.0,
        points: vec![Point { x: 200.0, y }],
    }
}

// Same velocity-Verlet integration used by the game. Missing local support falls back to
// zero acceleration, explicitly counted below; this is not a claim that gravity is absent.
fn predict(
    field: &LocalField,
    base: &PlanetMap,
    observation: &ShotObservation,
    ticks: usize,
) -> (Vec<Vec2>, usize, usize) {
    let first = &observation.points[0];
    let mut p = Vec2::new(f64::from(first.x), f64::from(first.y));
    let angle = observation.angle_degrees.to_radians();
    let mut v = Vec2::new(
        observation.velocity * angle.cos(),
        -observation.velocity * angle.sin(),
    );
    let mut out = vec![p];
    let mut supported = 0;
    let mut queries = 0;
    let mut acceleration = |position| {
        queries += 1;
        let result = field.acceleration(position);
        supported += usize::from(result.is_some());
        let residual = result.unwrap_or_default();
        let physical = base.acceleration(position).unwrap();
        Vec2::new(physical.x + residual.x, physical.y + residual.y)
    };
    let mut a = acceleration(p);
    let dt = 1.0 / WorldParameters::default().substeps_per_tick as f64;
    for _ in 1..ticks {
        for _ in 0..WorldParameters::default().substeps_per_tick {
            p.x += v.x * dt + 0.5 * a.x * dt * dt;
            p.y += v.y * dt + 0.5 * a.y * dt * dt;
            let next = acceleration(p);
            v.x += 0.5 * (a.x + next.x) * dt;
            v.y += 0.5 * (a.y + next.y) * dt;
            a = next;
        }
        out.push(p);
    }
    (out, supported, queries)
}
fn rmse(a: &[Vec2], b: &[Vec2]) -> f64 {
    (a.iter()
        .zip(b)
        .map(|(a, b)| (a.x - b.x).powi(2) + (a.y - b.y).powi(2))
        .sum::<f64>()
        / (2 * a.len()) as f64)
        .sqrt()
}
fn run(name: &str, truth: &PlanetMap, training_y: f32, held_y: f32) {
    let world = WorldParameters::default();
    let training = shot(training_y);
    let observation = simulate_shot(truth, &training, world, 2000).unwrap();
    assert_eq!(
        observation.termination,
        Termination::BattlefieldMargin,
        "training must be a completed flyby"
    );
    let flat_map = PlanetMap { planets: vec![] };
    let field = LocalField::from_path(&observation.points, &flat_map);
    let held = shot(held_y);
    let held_termination = simulate_shot(truth, &held, world, 2000).unwrap();
    assert!(
        held_termination.points.len() > 70,
        "held-out trajectory terminates before evaluation horizon: {:?}",
        held_termination.termination
    );
    let truth_positions = simulate_sampled_positions(truth, &held, world, 70).unwrap();
    let flat =
        simulate_sampled_positions(&PlanetMap { planets: vec![] }, &held, world, 70).unwrap();
    let (prediction, supported, queries) = predict(&field, &flat_map, &held, 70);
    let mut count = 0;
    let mut local_sq = 0.0;
    let mut flat_sq = 0.0;
    for p in &truth_positions {
        if let Some(a) = field.acceleration(*p) {
            let exact = truth.acceleration(*p).unwrap();
            count += 1;
            local_sq += (exact.x - a.x).powi(2) + (exact.y - a.y).powi(2);
            flat_sq += exact.x.powi(2) + exact.y.powi(2);
        }
    }
    let (local_field_rmse, flat_field_rmse) = if count == 0 {
        (None, None)
    } else {
        (
            Some((local_sq / (2 * count) as f64).sqrt()),
            Some((flat_sq / (2 * count) as f64).sqrt()),
        )
    };
    let matched_residual = LocalField::from_path(&observation.points, truth);
    let (matched_prediction, matched_support, matched_queries) =
        predict(&matched_residual, truth, &held, 70);
    let matched = rmse(&truth_positions, &matched_prediction);
    println!(
        "{name} matched_physical_plus_residual_trajectory_coordinate_rmse={matched:.8} supported_queries={matched_support}/{matched_queries}"
    );
    println!(
        "{name} train_y={training_y} held_y={held_y} training_samples={} supported_field_positions={count}/{} supported_prediction_queries={supported}/{queries} field_coordinate_rmse_local={local_field_rmse:?} field_coordinate_rmse_flat={flat_field_rmse:?} trajectory_coordinate_rmse_local={:.4} trajectory_coordinate_rmse_flat={:.4}",
        field.samples.len(),
        truth_positions.len(),
        rmse(&truth_positions, &prediction),
        rmse(&truth_positions, &flat)
    );
}
fn main() {
    let planet = PlanetMap {
        planets: vec![Planet {
            x: 600.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        }],
    };
    let flat = PlanetMap { planets: vec![] };
    run("flat_control", &flat, 400.0, 410.0);
    run("near_flyby", &planet, 400.0, 410.0);
    run("offset_flyby", &planet, 400.0, 350.0);
    run("far_flyby", &planet, 400.0, 250.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reconstruction_omits_untimed_endpoint() {
        let points = vec![
            Point { x: 0.0, y: 0.0 },
            Point { x: 1.0, y: 0.0 },
            Point { x: 4.0, y: 0.0 },
            Point { x: 9.0, y: 0.0 },
            Point { x: 1000.0, y: 0.0 },
        ];
        let field = LocalField::from_path(&points, &PlanetMap { planets: vec![] });
        assert_eq!(field.samples.len(), 2);
        assert_eq!(field.samples[0], (Vec2::new(1.0, 0.0), Vec2::new(2.0, 0.0)));
        assert_eq!(field.samples[1], (Vec2::new(4.0, 0.0), Vec2::new(2.0, 0.0)));
    }

    #[test]
    fn interpolation_is_local_and_preserves_constant_acceleration() {
        let field = LocalField {
            samples: vec![
                (Vec2::new(0.0, 0.0), Vec2::new(0.2, -0.3)),
                (Vec2::new(8.0, 0.0), Vec2::new(0.2, -0.3)),
            ],
        };
        let a = field.acceleration(Vec2::new(4.0, 0.0)).unwrap();
        assert!((a.x - 0.2).abs() < 1e-12 && (a.y + 0.3).abs() < 1e-12);
        assert_eq!(field.acceleration(Vec2::new(100.0, 0.0)), None);
        let one = LocalField {
            samples: vec![(Vec2::new(0.0, 0.0), Vec2::new(0.2, -0.3))],
        };
        assert!(one.acceleration(Vec2::new(SUPPORT_RADIUS, 0.0)).is_some());
        assert_eq!(
            one.acceleration(Vec2::new(SUPPORT_RADIUS + 0.001, 0.0)),
            None
        );
    }
}
