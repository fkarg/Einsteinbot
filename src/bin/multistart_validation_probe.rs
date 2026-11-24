//! Offline comparison of source starts and independent-path model selection.
//! The synthetic oracle generates and scores trajectories; it never reaches a fitter.
use einsteinbot::estimator::{
    SeedStrategy, fit_fixed_count, fit_greedy_source_counts_with_strategy, position_rmse,
    seed_planet_from_residuals_with_strategy,
};
use einsteinbot::map::PlanetMap;
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::protocol::{Planet, Point, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_sampled_positions_with_hypothesis,
    simulate_sampled_positions_with_local_field, simulate_shot,
};
use einsteinbot::strategy::MapBelief;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn random(state: &mut u64) -> f64 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    ((*state >> 11) as f64) / ((1u64 << 53) as f64)
}
fn field_rmse(
    belief: Option<&MapBelief>,
    shot: &ShotObservation,
    world: WorldParameters,
) -> Option<f64> {
    let belief = belief?;
    let count = shot.points.len().checked_sub(1)?;
    let predictions = match (&belief.field_hypothesis, &belief.hypothesis_local_field) {
        (Some(h), Some(f)) => {
            simulate_sampled_positions_with_hypothesis(&belief.map, f, h, shot, world, count)
        }
        _ => simulate_sampled_positions_with_local_field(
            &belief.map,
            &belief.local_field,
            shot,
            world,
            count,
        ),
    }
    .ok()?;
    let mse = shot.points[1..count]
        .iter()
        .zip(&predictions[1..])
        .map(|(p, q)| (f64::from(p.x) - q.x).powi(2) + (f64::from(p.y) - q.y).powi(2))
        .sum::<f64>()
        / (2 * (count - 1)) as f64;
    Some(mse.sqrt())
}
fn main() {
    let world = WorldParameters::default();
    let flat = PlanetMap { planets: vec![] };
    let mut arguments = std::env::args().skip(1).collect::<Vec<_>>();
    let strategy = if let Some(index) = arguments.iter().position(|arg| arg == "--seed") {
        assert!(
            index + 1 < arguments.len(),
            "--seed requires grid or bearing"
        );
        let value = arguments.remove(index + 1);
        arguments.remove(index);
        match value.as_str() {
            "grid" => SeedStrategy::Grid,
            "bearing" => SeedStrategy::Bearing,
            _ => panic!("--seed requires grid or bearing"),
        }
    } else {
        SeedStrategy::Grid
    };
    let only = arguments
        .first()
        .filter(|arg| *arg != "all")
        .map(|arg| arg.parse::<u64>().expect("seed"));
    let base = arguments
        .get(1)
        .map(|arg| arg.parse::<u64>().expect("base seed"))
        .unwrap_or(2026112200);
    let cases = if arguments.len() > 1 { 4 } else { 8 };
    for source_count in 0..=3 {
        for case in 0..cases {
            let seed = base + (source_count as u64) * 100 + case;
            if only.is_some_and(|value| value != seed) {
                continue;
            }
            let mut rng = seed;
            let mut truth = PlanetMap { planets: vec![] };
            while truth.planets.len() < source_count {
                let p = Planet {
                    x: 450.0 + 950.0 * random(&mut rng),
                    y: 180.0 + 700.0 * random(&mut rng),
                    radius: 20.0 + 20.0 * random(&mut rng),
                    mass: 0.0,
                };
                if truth
                    .planets
                    .iter()
                    .all(|q| (p.x - q.x).hypot(p.y - q.y) > p.radius + q.radius + 100.0)
                {
                    truth.planets.push(Planet {
                        mass: p.radius.powi(3) / 10.0,
                        ..p
                    });
                }
            }
            let mut paths = Vec::new();
            for index in 0..60 {
                let reverse = index % 2 == 1;
                let y = 80.0 + 880.0 * random(&mut rng);
                let angle = if reverse {
                    180.0 + 24.0 * (random(&mut rng) - 0.5)
                } else {
                    24.0 * (random(&mut rng) - 0.5)
                };
                let mut shot = ShotObservation {
                    player_id: 1,
                    angle_degrees: angle,
                    velocity: 8.0,
                    points: vec![Point {
                        x: if reverse { 1720.0 } else { 150.0 },
                        y: y as f32,
                    }],
                };
                let path = simulate_shot(&truth, &shot, world, 2000).unwrap();
                if path.termination == Termination::BattlefieldMargin && path.points.len() > 30 {
                    shot.points = path.points;
                    paths.push(shot);
                    if paths.len() == 4 {
                        break;
                    }
                }
            }
            if paths.len() < 4 {
                println!("case={seed} insufficient_paths={}", paths.len());
                continue;
            }
            let training = &paths[..2];
            let validation = &paths[2..3];
            let heldout = &paths[3..4];
            let flat_val = position_rmse(&flat, validation, world).unwrap();
            let flat_test = position_rmse(&flat, heldout, world).unwrap();
            let oracle_test = position_rmse(&truth, heldout, world).unwrap();
            let started = Instant::now();
            let mut candidates = vec![("flat", flat.clone(), flat_val)];
            if let Ok(search) = fit_greedy_source_counts_with_strategy(
                training, validation, world, 2, 30.0, 80.0, strategy,
            ) {
                for c in search.candidates {
                    let label = if c.result.map.planets.len() == 1 {
                        "greedy1"
                    } else {
                        "greedy2"
                    };
                    candidates.push((label, c.result.map, c.validation_rmse.unwrap()));
                }
            }
            let mut independent = Vec::new();
            for shot in training {
                if let Ok(seed) = seed_planet_from_residuals_with_strategy(
                    &flat,
                    std::slice::from_ref(shot),
                    world,
                    30.0,
                    80.0,
                    strategy,
                    &AtomicBool::new(false),
                ) {
                    if let Ok(fit) = fit_fixed_count(
                        &PlanetMap {
                            planets: vec![seed],
                        },
                        std::slice::from_ref(shot),
                        world,
                    ) {
                        independent.push(fit.map.planets[0].clone());
                    }
                }
            }
            if independent.len() == 2 {
                if let Ok(fit) = fit_fixed_count(
                    &PlanetMap {
                        planets: independent,
                    },
                    training,
                    world,
                ) {
                    if let Ok(value) = position_rmse(&fit.map, validation, world) {
                        candidates.push(("combined2", fit.map, value));
                    }
                }
            }
            // Apply the existing field-validation heuristic to each complexity step:
            // require >0.01 absolute and >20% relative improvement on path 3.
            let mut selected = &candidates[0];
            for count in 1..=2 {
                if let Some(candidate) = candidates
                    .iter()
                    .filter(|c| c.1.planets.len() == count)
                    .min_by(|a, b| a.2.total_cmp(&b.2))
                {
                    if candidate.2 < selected.2 * 0.8 && selected.2 - candidate.2 > 0.01 {
                        selected = candidate;
                    }
                }
            }
            let test = position_rmse(&selected.1, heldout, world).unwrap();
            // Give the selected candidate path 3 for parameter refinement only after
            // selection. Path 4 remains hidden from both fitting and selection.
            let refined = if selected.1.planets.is_empty() {
                selected.1.clone()
            } else {
                fit_fixed_count(&selected.1, &paths[..3], world)
                    .map(|fit| fit.map)
                    .unwrap_or_else(|_| selected.1.clone())
            };
            let refined_test = position_rmse(&refined, heldout, world).unwrap();
            if !selected.1.planets.is_empty() {
                let before_train = position_rmse(&selected.1, training, world).unwrap();
                let after_train = position_rmse(&refined, training, world).unwrap();
                let after_validation = position_rmse(&refined, validation, world).unwrap();
                println!(
                    "  refit_check case={seed} before_train={before_train:.5} after_train={after_train:.5} before_validation={:.5} after_validation={after_validation:.5}",
                    selected.2
                );
                for (i, path) in paths[..3].iter().enumerate() {
                    println!(
                        "  refit_path case={seed} path={} before={:.5} after={:.5}",
                        i + 1,
                        position_rmse(&selected.1, std::slice::from_ref(path), world).unwrap(),
                        position_rmse(&refined, std::slice::from_ref(path), world).unwrap()
                    );
                }
            }
            let proposal_ms = started.elapsed().as_millis();
            // Future-path scores are reported only after all candidate fitting and
            // selection. They must not influence retention or refinement.
            for (label, candidate, validation_rmse) in &candidates {
                let future = position_rmse(candidate, heldout, world).ok();
                let belief = MapBelief::new(candidate.clone(), None);
                let field = einsteinbot::field_score::score_field(
                    &truth,
                    &belief,
                    &paths[..3],
                    world,
                    einsteinbot::field_score::FieldModel::Physical,
                );
                let explored = field
                    .as_ref()
                    .and_then(|s| s.explored.as_ref())
                    .and_then(|s| s.vector_rmse);
                let unexplored = field
                    .as_ref()
                    .and_then(|s| s.unexplored.as_ref())
                    .and_then(|s| s.vector_rmse);
                let whole = field
                    .as_ref()
                    .and_then(|s| s.whole.as_ref())
                    .and_then(|s| s.vector_rmse);
                println!(
                    "  candidate case={seed} seed={strategy:?} label={label} count={} validation={validation_rmse:.6} future={future:?} explored={explored:?} unexplored={unexplored:?} whole={whole:?}",
                    candidate.planets.len()
                );
            }
            let mut online = OnlineEstimator::with_seed_strategy(strategy);
            let cancel = AtomicBool::new(false);
            let live_started = Instant::now();
            let mut live_two = None;
            for (index, shot) in paths[..3].iter().enumerate() {
                let start = &shot.points[0];
                online.observe(
                    &ServerEvent::PlayerPosition {
                        player_id: 1,
                        x: start.x,
                        y: start.y,
                    },
                    &cancel,
                );
                online.observe(
                    &ServerEvent::ShotFin {
                        player_id: 1,
                        angle: shot.angle_degrees,
                        velocity: shot.velocity,
                        points: shot.points.clone(),
                    },
                    &cancel,
                );
                if index == 1 {
                    live_two = field_rmse(online.belief(), &heldout[0], world);
                }
            }
            let live_ms = live_started.elapsed().as_millis();
            let live = field_rmse(online.belief(), &heldout[0], world);
            let other_strategy = if strategy == SeedStrategy::Grid {
                SeedStrategy::Bearing
            } else {
                SeedStrategy::Grid
            };
            let mut bearing = OnlineEstimator::with_seed_strategy(other_strategy);
            for shot in &paths[..3] {
                let start = &shot.points[0];
                bearing.observe(
                    &ServerEvent::PlayerPosition {
                        player_id: 1,
                        x: start.x,
                        y: start.y,
                    },
                    &cancel,
                );
                bearing.observe(
                    &ServerEvent::ShotFin {
                        player_id: 1,
                        angle: shot.angle_degrees,
                        velocity: shot.velocity,
                        points: shot.points.clone(),
                    },
                    &cancel,
                );
            }
            let scores = [online.belief(), bearing.belief()].map(|belief| {
                belief.and_then(|belief| {
                    einsteinbot::field_score::score_field(
                        &truth,
                        belief,
                        &paths[..3],
                        world,
                        einsteinbot::field_score::FieldModel::Validated,
                    )
                })
            });
            let names = if strategy == SeedStrategy::Grid {
                ["grid", "bearing"]
            } else {
                ["bearing", "grid"]
            };
            for (name, score) in names.into_iter().zip(scores) {
                if let Some(score) = score {
                    println!(
                        "  field_score case={seed} seed={name} explored={:?} unexplored={:?} whole={:?}",
                        score.explored.as_ref().and_then(|s| s.vector_rmse),
                        score.unexplored.as_ref().and_then(|s| s.vector_rmse),
                        score.whole.as_ref().and_then(|s| s.vector_rmse)
                    );
                    println!(
                        "  field_score_detail case={seed} seed={name} {}",
                        serde_json::to_string(&score).unwrap()
                    );
                }
            }
            if let Some(belief) = online.belief() {
                if live.is_some_and(|error| error > 10000.0) {
                    println!(
                        "  large_error_case={seed} truth={:?} physical={:?} hypothesis={:?} holdout_start={:?}",
                        truth.planets,
                        belief.map.planets,
                        belief.field_hypothesis.as_ref().map(|h| (h.x, h.y, h.mass)),
                        heldout[0].points.first()
                    );
                    if let (Some(h), Some(f)) =
                        (&belief.field_hypothesis, &belief.hypothesis_local_field)
                    {
                        let predicted = simulate_sampled_positions_with_hypothesis(
                            &belief.map,
                            f,
                            h,
                            &heldout[0],
                            world,
                            heldout[0].points.len() - 1,
                        )
                        .unwrap();
                        let observed_nearest = heldout[0]
                            .points
                            .iter()
                            .map(|p| (f64::from(p.x) - h.x).hypot(f64::from(p.y) - h.y))
                            .fold(f64::INFINITY, f64::min);
                        let predicted_nearest = predicted
                            .iter()
                            .map(|p| (p.x - h.x).hypot(p.y - h.y))
                            .fold(f64::INFINITY, f64::min);
                        let max_step = predicted
                            .windows(2)
                            .map(|p| (p[1].x - p[0].x).hypot(p[1].y - p[0].y))
                            .fold(0.0, f64::max);
                        println!(
                            "  large_error_source_distances observed_min={observed_nearest:.3} predicted_tick_min={predicted_nearest:.3} predicted_max_tick_step={max_step:.3}"
                        );
                        let mut source_only = belief.clone();
                        source_only.hypothesis_local_field =
                            Some(einsteinbot::local_field::LocalField::default());
                        let mut without_source = belief.clone();
                        without_source.field_hypothesis = None;
                        println!(
                            "  large_error_ablation source_only={:?} no_source={:?}",
                            field_rmse(Some(&source_only), &heldout[0], world),
                            field_rmse(Some(&without_source), &heldout[0], world)
                        );
                        let bounded =
                            einsteinbot::simulation::simulate_targeting_shot_with_hypothesis(
                                &belief.map,
                                f,
                                h,
                                &heldout[0],
                                world,
                                2000,
                                &[],
                                0.0,
                            )
                            .unwrap();
                        println!(
                            "  targeting_sim predicted_termination={:?} predicted_points={} observed_points={}",
                            bounded.termination,
                            bounded.points.len(),
                            heldout[0].points.len()
                        );
                    }
                }
            }
            let nearest = heldout[0]
                .points
                .iter()
                .flat_map(|point| {
                    truth
                        .planets
                        .iter()
                        .map(move |p| (f64::from(point.x) - p.x).hypot(f64::from(point.y) - p.y))
                })
                .fold(f64::INFINITY, f64::min);
            println!(
                "case={seed} truth_count={source_count} fit_seed={strategy:?} confirm={} selected={} validation={:.5} test={test:.5} refined_test={refined_test:.5} live2={live_two:?} live3={live:?} flat={flat_test:.5} oracle={oracle_test:.5} nearest_truth={nearest:.1} fit_ms={proposal_ms} live_ms={live_ms} models={} val_scores={:?}",
                case >= 4 || arguments.len() > 1,
                selected.0,
                selected.2,
                candidates.len(),
                candidates.iter().map(|c| (c.0, c.2)).collect::<Vec<_>>()
            );
        }
    }
}
