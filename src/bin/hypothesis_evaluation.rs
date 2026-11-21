//! Compare field-only hypotheses against flat/local predictions on disjoint shots.
//! Capture oracle planets are used only to score the collision termination.
use einsteinbot::event_store::EventLog;
use einsteinbot::field_hypothesis::{FieldHypothesis, propose_field};
use einsteinbot::local_field::LocalField;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, WorldParameters, simulate_sampled_positions,
    simulate_sampled_positions_with_local_field, simulate_shot,
};
use std::sync::atomic::AtomicBool;

fn score(observed: &ShotObservation, prediction: &[Vec2]) -> f64 {
    let n = observed.points.len() - 1;
    (observed.points[1..n]
        .iter()
        .zip(&prediction[1..])
        .map(|(p, q)| (f64::from(p.x) - q.x).powi(2) + (f64::from(p.y) - q.y).powi(2))
        .sum::<f64>()
        / (2 * (n - 1)) as f64)
        .sqrt()
}
fn compare(
    label: &str,
    heldout: &ShotObservation,
    hypothesis: &FieldHypothesis,
    local: &LocalField,
    oracle: Option<&PlanetMap>,
) {
    let world = WorldParameters::default();
    let n = heldout.points.len() - 1;
    if n < 3 {
        return;
    }
    let flat = PlanetMap { planets: vec![] };
    // Zero radius adds gravity only and cannot represent actual collision boundaries.
    let source = PlanetMap {
        planets: vec![Planet {
            x: hypothesis.x,
            y: hypothesis.y,
            mass: hypothesis.mass,
            radius: 0.,
        }],
    };
    let pred_flat = simulate_sampled_positions(&flat, heldout, world, n).unwrap();
    let pred_source = simulate_sampled_positions(&source, heldout, world, n).unwrap();
    let pred_local =
        simulate_sampled_positions_with_local_field(&flat, local, heldout, world, n).unwrap();
    let dist = heldout.points[1..n]
        .iter()
        .map(|p| hypothesis.support_distance(Vec2::new(f64::from(p.x), f64::from(p.y))))
        .collect::<Vec<_>>();
    let nearby = dist.iter().filter(|d| **d <= 40.).count();
    let mean_dist = dist.iter().sum::<f64>() / dist.len() as f64;
    if let Some(truth) = oracle {
        for (lower, upper) in [(0., 40.), (40., 150.), (150., f64::INFINITY)] {
            let mut count = 0usize;
            let mut flat_err = 0.;
            let mut local_err = 0.;
            let mut source_err = 0.;
            for (point, distance) in heldout.points[1..n].iter().zip(&dist) {
                if *distance < lower || *distance >= upper {
                    continue;
                }
                let p = Vec2::new(f64::from(point.x), f64::from(point.y));
                let Some(predicted) = hypothesis.acceleration(p) else {
                    continue;
                };
                let Ok(actual) = truth.acceleration(p) else {
                    continue;
                };
                let local = local.residual(p).unwrap_or_default();
                flat_err += actual.norm_squared();
                local_err += (actual.x - local.x).powi(2) + (actual.y - local.y).powi(2);
                source_err += (actual.x - predicted.x).powi(2) + (actual.y - predicted.y).powi(2);
                count += 1;
            }
            if count > 0 {
                println!(
                    "  field_distance=[{lower},{upper}) n={count} flat_vec_rmse={:.5} local_vec_rmse={:.5} hypothesis_vec_rmse={:.5}",
                    (flat_err / count as f64).sqrt(),
                    (local_err / count as f64).sqrt(),
                    (source_err / count as f64).sqrt()
                );
            }
        }
    }
    let collision = oracle
        .and_then(|map| simulate_shot(map, heldout, world, 2000).ok())
        .map(|s| s.termination);
    println!(
        "{label} samples={n} flat_rmse={:.3} local_rmse={:.3} hypothesis_rmse={:.3} within40={nearby}/{} mean_support_distance={mean_dist:.1} oracle_termination={collision:?} field_hypothesis_has_collision_geometry=false",
        score(heldout, &pred_flat),
        score(heldout, &pred_local),
        score(heldout, &pred_source),
        dist.len()
    );
}
fn synthetic() {
    let truth = PlanetMap {
        planets: vec![Planet {
            x: 600.,
            y: 500.,
            radius: 30.,
            mass: 2700.,
        }],
    };
    let generate = |y| {
        let mut shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.,
            velocity: 8.,
            points: vec![Point { x: 200., y }],
        };
        shot.points = simulate_shot(&truth, &shot, WorldParameters::default(), 2000)
            .unwrap()
            .points;
        shot
    };
    let training = generate(400.);
    let local = LocalField::from_observations(
        &PlanetMap { planets: vec![] },
        std::slice::from_ref(&training),
    );
    let hypothesis = propose_field(
        &training,
        WorldParameters::default(),
        &AtomicBool::new(false),
    )
    .expect("synthetic flyby");
    println!(
        "synthetic proposal x={:.3} y={:.3} mass={:.3} validated_tail_rmse={:.3}",
        hypothesis.x, hypothesis.y, hypothesis.mass, hypothesis.continuation_rmse
    );
    for y in [390., 350., 200., 800.] {
        compare(
            &format!("synthetic y={y}"),
            &generate(y),
            &hypothesis,
            &local,
            Some(&truth),
        );
    }
}
fn capture() {
    let events = EventLog::read_capture("einsteinbot.sqlite3", 29).expect("capture 29 unavailable");
    let mut training: Option<(i64, ShotObservation, FieldHypothesis)> = None;
    let mut pending: Vec<(i64, u32, f64, f64)> = Vec::new();
    let mut oracle = None;
    let mut count = 0;
    for row in events {
        let RecordedEvent::Inbound(event) = row.event else {
            continue;
        };
        match event {
            ServerEvent::Planets { planets } => {
                oracle = Some(PlanetMap { planets });
                training = None;
                pending.clear()
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => pending.push((row.sequence, player_id, angle, velocity)),
            event @ ServerEvent::ShotFin { .. } => {
                let shot = ShotObservation::try_from(&event).unwrap();
                let launch = pending
                    .iter()
                    .position(|(_, id, angle, velocity)| {
                        (*id, *angle, *velocity)
                            == (shot.player_id, shot.angle_degrees, shot.velocity)
                    })
                    .map(|index| pending.remove(index).0);
                if training.is_none() {
                    if let Some(hypothesis) =
                        propose_field(&shot, WorldParameters::default(), &AtomicBool::new(false))
                    {
                        println!(
                            "capture training_fin_seq={} x={:.2} y={:.2} mass={:.2} continuation_rmse={:.2} baseline_tail_rmse={:.2}",
                            row.sequence,
                            hypothesis.x,
                            hypothesis.y,
                            hypothesis.mass,
                            hypothesis.continuation_rmse,
                            hypothesis.baseline_continuation_rmse
                        );
                        training = Some((row.sequence, shot, hypothesis));
                    }
                } else if let Some((seq, train, hypothesis)) = training.as_ref() {
                    if launch.is_some_and(|launch_seq| launch_seq > *seq) {
                        let local = LocalField::from_observations(
                            &PlanetMap { planets: vec![] },
                            std::slice::from_ref(train),
                        );
                        compare(
                            &format!("capture train_seq={seq} heldout_fin_seq={}", row.sequence),
                            &shot,
                            hypothesis,
                            &local,
                            oracle.as_ref(),
                        );
                        count += 1;
                        if count == 6 {
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    println!("capture evaluated={count} (only launches after training completion)");
}
fn main() {
    synthetic();
    capture()
}
