//! Measurements that justify the estimator's parameters. They are ignored tests that print
//! results; run with `cargo test --release measure -- --ignored --nocapture`. Truth (disclosed
//! planets or a synthetic world) is used only to score, never as fit input.
use super::fit::Fit;
use super::*;
use crate::event_store::EventLog;
use crate::protocol::{Planet, RecordedEvent};
use crate::simulation::simulate_shot;

fn truth_theta(planets: &[Planet]) -> Vec<[f64; 3]> {
    planets.iter().map(|p| [p.x, p.y, p.radius]).collect()
}

/// Sum of squared whitened window residuals per coordinate, divided by the row count: 1.0 when the
/// model and the noise are exact. `None` for a path with no usable rows.
fn chi_square_per_coordinate(shot: &Shot, truth: &[Planet]) -> Option<f64> {
    shot.path.as_ref()?;
    let never = AtomicBool::new(false);
    let fit = Fit::new(
        std::slice::from_ref(shot),
        WorldParameters::default(),
        &never,
        false,
        Vec::new(),
    );
    let rows = fit.unexplained(&truth_theta(truth));
    let chi2: f64 = rows.iter().map(|w| w.significance.powi(2)).sum();
    (!rows.is_empty()).then(|| chi2 / (2.0 * rows.len() as f64))
}

fn print_quantiles(label: &str, mut values: Vec<f64>) {
    values.sort_by(f64::total_cmp);
    let q = |f: f64| values[((values.len() - 1) as f64 * f) as usize];
    println!(
        "{label}: paths {} median {:.2} p75 {:.2} p90 {:.2} p95 {:.2} max {:.1}",
        values.len(),
        q(0.5),
        q(0.75),
        q(0.9),
        q(0.95),
        q(1.0)
    );
}

fn uniform(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 11) as f64 / (1u64 << 53) as f64
}

/// Residual scale of the windowed comparison against the disclosed truth map on every capture
/// in `CAPTURE_DB` (default `einsteinbot.sqlite3`) that discloses planets, with only the f32
/// rounding noise and the interpolation variance. This is the measurement behind
/// `MODEL_NOISE_SD`.
#[test]
#[ignore]
fn measure_residual_scale_on_live_captures() {
    let db = std::env::var("CAPTURE_DB").unwrap_or_else(|_| "einsteinbot.sqlite3".into());
    let world = WorldParameters::default();
    let mut all = Vec::new();
    for id in 1..=60 {
        let Ok(events) = EventLog::read_capture(&db, id) else {
            continue;
        };
        let mut truth: Option<Vec<Planet>> = None;
        let mut seen: Vec<ShotObservation> = Vec::new();
        for stored in events {
            let RecordedEvent::Inbound(event) = stored.event else {
                continue;
            };
            match &event {
                ServerEvent::Planets { planets } => {
                    truth = (!planets.is_empty()).then(|| planets.clone());
                    seen.clear();
                }
                ServerEvent::ShotFin { .. } => {
                    let (Some(planets), Ok(obs)) = (&truth, ShotObservation::try_from(&event))
                    else {
                        continue;
                    };
                    if seen.contains(&obs) {
                        continue;
                    }
                    seen.push(obs.clone());
                    all.extend(chi_square_per_coordinate(
                        &Shot::new(obs, None, world),
                        planets,
                    ));
                }
                _ => {}
            }
        }
    }
    print_quantiles("live captures, chi-square per coordinate", all);
}

/// The same statistic on synthetic paths simulated by the bot's own engine copy: it must be near 1
/// everywhere, including near passes.
#[test]
#[ignore]
fn measure_residual_scale_on_synthetic_paths() {
    let world = WorldParameters::default();
    let mut state = 99u64;
    let mut values = Vec::new();
    for _ in 0..60 {
        let planets: Vec<Planet> = (0..3)
            .map(|_| {
                let radius = 20.0 + 20.0 * uniform(&mut state);
                Planet {
                    x: 100.0 + 1700.0 * uniform(&mut state),
                    y: 100.0 + 800.0 * uniform(&mut state),
                    radius,
                    mass: radius.powi(3) / 10.0,
                }
            })
            .collect();
        let truth = PlanetMap {
            planets: planets.clone(),
        };
        for _ in 0..4 {
            let shot = ShotObservation {
                player_id: 1,
                angle_degrees: 360.0 * uniform(&mut state),
                velocity: 2.0 + 20.0 * uniform(&mut state),
                points: vec![Point {
                    x: (100.0 + 1700.0 * uniform(&mut state)) as f32,
                    y: (100.0 + 800.0 * uniform(&mut state)) as f32,
                }],
            };
            let Ok(path) = simulate_shot(&truth, &shot, world, 100) else {
                continue;
            };
            let obs = ShotObservation {
                points: path.points,
                ..shot
            };
            values.extend(chi_square_per_coordinate(
                &Shot::new(obs, None, world),
                &planets,
            ));
        }
    }
    print_quantiles("synthetic paths, chi-square per coordinate", values);
}

/// Position RMSE of a belief's map on a path it has not seen, as in `shot_position_rmse`.
fn held_out_rmse(map: &PlanetMap, shot: &ShotObservation) -> Option<f64> {
    shot_position_rmse(map, shot, WorldParameters::default())
        .ok()
        .flatten()
}

/// Held-out check on recorded captures: each completed path is predicted by the belief built from
/// the earlier paths only, and compared with the flat map. Truth plays no part.
#[test]
#[ignore]
fn measure_held_out_on_learning_captures() {
    let db = std::env::var("CAPTURE_DB").unwrap_or_else(|_| "learning-scenarios.sqlite3".into());
    let ids: Vec<i64> = std::env::var("CAPTURES")
        .map(|v| v.split(',').map(|i| i.parse().unwrap()).collect())
        .unwrap_or_else(|_| vec![2, 6, 10]);
    let never = AtomicBool::new(false);
    for id in ids {
        let events = EventLog::read_capture(&db, id).unwrap();
        let mut estimator = Estimator::default();
        let (mut learned, mut flat) = (Vec::new(), Vec::new());
        let mut planets = 0;
        for stored in events {
            let RecordedEvent::Inbound(event) = stored.event else {
                continue;
            };
            if let (ServerEvent::ShotFin { .. }, Some(belief)) = (&event, estimator.belief()) {
                if let Ok(obs) = ShotObservation::try_from(&event) {
                    learned.extend(held_out_rmse(&belief.map, &obs));
                    flat.extend(held_out_rmse(&PlanetMap { planets: vec![] }, &obs));
                }
            }
            if let Some(belief) = estimator.observe(&event, &never) {
                planets = belief.map.planets.len();
            }
        }
        println!(
            "capture {id}: final planets {planets}; held-out position RMSE per later path learned {:.4?} flat {:.4?}",
            learned, flat
        );
    }
}

/// Flat-world false-planet rate over generated flat paths: how many of `WORLDS` flat worlds, each
/// with `PATHS` paths, end with a planet, for several amounts of unmodelled position noise (the
/// noise sd in world units; 0 is only the f32 rounding of the data).
#[test]
#[ignore]
fn measure_flat_false_planet_rate() {
    let world = WorldParameters::default();
    let never = AtomicBool::new(false);
    for noise in [0.0, 1e-3, 1e-2, 1e-1] {
        let mut state = 7u64;
        let (mut with_planet, worlds) = (0, 16);
        for _ in 0..worlds {
            let mut estimator = Estimator::default();
            let mut belief = None;
            for _ in 0..4 {
                let start = Point {
                    x: (150.0 + 1500.0 * uniform(&mut state)) as f32,
                    y: (150.0 + 700.0 * uniform(&mut state)) as f32,
                };
                let shot = ShotObservation {
                    player_id: 1,
                    angle_degrees: 360.0 * uniform(&mut state),
                    velocity: 2.0 + 20.0 * uniform(&mut state),
                    points: vec![start],
                };
                let Ok(mut path) = simulate_shot(&PlanetMap { planets: vec![] }, &shot, world, 80)
                else {
                    continue;
                };
                for point in path.points.iter_mut() {
                    let gauss =
                        |state: &mut u64| (0..12).map(|_| uniform(state)).sum::<f64>() - 6.0;
                    point.x += (noise * gauss(&mut state)) as f32;
                    point.y += (noise * gauss(&mut state)) as f32;
                }
                belief = estimator
                    .observe(
                        &ServerEvent::ShotFin {
                            player_id: 1,
                            angle: shot.angle_degrees,
                            velocity: shot.velocity,
                            points: path.points,
                        },
                        &never,
                    )
                    .or(belief);
            }
            with_planet += usize::from(belief.is_some_and(|b| !b.map.planets.is_empty()));
        }
        println!(
            "extra noise sd {noise}: {with_planet} of {worlds} flat worlds ended with a planet"
        );
    }
}

/// Per-update time and planets while replaying a capture.
#[test]
#[ignore]
fn measure_update_cost() {
    let db = std::env::var("CAPTURE_DB").unwrap_or_else(|_| "learning-scenarios.sqlite3".into());
    let id: i64 = std::env::var("CAPTURE").map_or(18, |v| v.parse().unwrap());
    let max: usize = std::env::var("MAX_COMPLETED").map_or(100, |v| v.parse().unwrap());
    let never = AtomicBool::new(false);
    let mut estimator = Estimator::default();
    if let Ok(v) = std::env::var("MAX_PLANETS") {
        estimator.max_planets = v.parse().unwrap();
    }
    if let Ok(v) = std::env::var("BUDGET_MS") {
        estimator.time_budget = Duration::from_millis(v.parse().unwrap());
    }
    let mut completed = 0;
    let mut truth: Vec<Planet> = Vec::new();
    for stored in EventLog::read_capture(&db, id).unwrap() {
        let RecordedEvent::Inbound(event) = stored.event else {
            continue;
        };
        if let ServerEvent::Planets { planets } = &event {
            truth = planets.clone();
        }
        let is_shot = matches!(event, ServerEvent::ShotFin { .. });
        let start = Instant::now();
        let belief = estimator.observe(&event, &never);
        if let (true, Some(belief)) = (is_shot, belief) {
            completed += 1;
            let elapsed = start.elapsed();
            let never = AtomicBool::new(false);
            let fit = Fit::new(
                &estimator.shots,
                WorldParameters::default(),
                &never,
                false,
                Vec::new(),
            );
            let rows: usize = estimator
                .shots
                .iter()
                .filter_map(|s| s.path.as_ref())
                .map(|p| 2 * p.row_count())
                .sum();
            let theta: Vec<[f64; 3]> = belief
                .map
                .planets
                .iter()
                .map(|p| [p.x, p.y, p.radius])
                .collect();
            println!(
                "   chi2/row model {:.2?} truth {:.2?} (rows {rows})",
                fit.chi2(&theta).map(|c| c / rows as f64),
                fit.chi2(&truth_theta(&truth)).map(|c| c / rows as f64)
            );
            println!(
                "path {completed}: {:?} planets {} limit {:?}",
                elapsed,
                belief.map.planets.len(),
                belief.search_limit
            );
        }
        if completed >= max {
            break;
        }
    }
}
