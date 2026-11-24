//! Offline chronological comparison of gravity-source recombination on captured paths.
use einsteinbot::estimator::{
    SeedStrategy, fit_fixed_count, fit_greedy_source_counts_with_strategy, position_rmse,
    seed_planet_from_residuals_with_strategy, shot_observations,
};
use einsteinbot::event_store::EventLog;
use einsteinbot::map::PlanetMap;
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::planet_fit::{ContactConstraint, fit_planet_map};
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

fn main() {
    let mut args: Vec<_> = std::env::args().skip(1).collect();
    let strategy = if let Some(index) = args.iter().position(|arg| arg == "--seed") {
        assert!(index + 1 < args.len(), "--seed requires grid or bearing");
        let value = args.remove(index + 1);
        args.remove(index);
        match value.as_str() {
            "grid" => SeedStrategy::Grid,
            "bearing" => SeedStrategy::Bearing,
            _ => panic!("--seed requires grid or bearing"),
        }
    } else {
        SeedStrategy::Grid
    };
    let fit_greedy_source_counts = |train: &[ShotObservation],
                                    validation: &[ShotObservation],
                                    world,
                                    max_sources,
                                    radius,
                                    spacing| {
        fit_greedy_source_counts_with_strategy(
            train,
            validation,
            world,
            max_sources,
            radius,
            spacing,
            strategy,
        )
    };
    let seed_planet_from_residuals =
        |map: &PlanetMap, shots: &[ShotObservation], world, radius, spacing| {
            seed_planet_from_residuals_with_strategy(
                map,
                shots,
                world,
                radius,
                spacing,
                strategy,
                &AtomicBool::new(false),
            )
        };
    let ids: Vec<i64> = if args.is_empty() {
        vec![11, 12, 13, 14, 15, 17]
    } else {
        args.iter()
            .map(|s| s.parse().expect("capture id"))
            .collect()
    };
    let world = WorldParameters::default();
    for id in ids {
        let events = EventLog::read_capture("learning-scenarios.sqlite3", id).unwrap();
        let shots = shot_observations(&events);
        if shots.len() < 3 {
            continue;
        }
        // The final recorded shot is the scenario trial; no future paths train the fit.
        let train = &shots[..shots.len() - 1];
        let holdout = &shots[shots.len() - 1..];
        let flat = PlanetMap { planets: vec![] };
        println!(
            "capture={id} fit_seed={strategy:?} train_paths={} holdout_paths=1 flat_rmse={:.6}",
            train.len(),
            position_rmse(&flat, holdout, world).unwrap()
        );
        let started = Instant::now();
        match fit_greedy_source_counts(train, holdout, world, 3, 30.0, 80.0) {
            Ok(search) => {
                for c in search.candidates {
                    println!(
                        "  greedy count={} train={:.6} holdout={:.6} rank={}/{} search_total_ms={} centers={:?}",
                        c.result.map.planets.len(),
                        c.result.residual_rmse,
                        c.validation_rmse.unwrap(),
                        c.result.rank,
                        3 * c.result.map.planets.len(),
                        started.elapsed().as_millis(),
                        c.result
                            .map
                            .planets
                            .iter()
                            .map(|p| (p.x, p.y, p.mass))
                            .collect::<Vec<_>>()
                    );
                }
            }
            Err(e) => println!("  greedy_error={e}"),
        }
        if id == 11 {
            let one = fit_greedy_source_counts(train, holdout, world, 1, 30.0, 80.0)
                .unwrap()
                .candidates
                .remove(0)
                .result;
            for mass in [1e-8, 1.0, 100.0] {
                let mut initial = one.map.clone();
                initial.planets.push(einsteinbot::protocol::Planet {
                    x: 100.0,
                    y: 100.0,
                    radius: 30.0,
                    mass,
                });
                let before = position_rmse(&initial, train, world).unwrap();
                match fit_fixed_count(&initial, train, world) {
                    Ok(fit) => println!(
                        "  nested_warm_start mass={mass} original_train={:.6} start_train={before:.6} final_train={:.6} holdout={:.6}",
                        one.residual_rmse,
                        fit.residual_rmse,
                        position_rmse(&fit.map, holdout, world).unwrap()
                    ),
                    Err(e) => println!(
                        "  nested_warm_start mass={mass} start_train={before:.6} error={e}"
                    ),
                }
            }
        }
        let started = Instant::now();
        let mut proposals = Vec::new();
        for shot in &train[..2] {
            let seed = match seed_planet_from_residuals(
                &flat,
                std::slice::from_ref(shot),
                world,
                30.0,
                80.0,
            ) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let map = PlanetMap {
                planets: vec![seed],
            };
            if let Ok(fit) = fit_fixed_count(&map, std::slice::from_ref(shot), world) {
                proposals.push(fit.map.planets[0].clone());
            }
        }
        if proposals.len() == 2 {
            let separation =
                (proposals[0].x - proposals[1].x).hypot(proposals[0].y - proposals[1].y);
            println!(
                "  independent_seeds separation={separation:.3} centers={:?}",
                proposals
                    .iter()
                    .map(|p| (p.x, p.y, p.mass))
                    .collect::<Vec<_>>()
            );
            match fit_fixed_count(&PlanetMap { planets: proposals }, train, world) {
                Ok(fit) => println!(
                    "  recombined count=2 train={:.6} holdout={:.6} rank={}/6 elapsed_ms={} centers={:?}",
                    fit.residual_rmse,
                    position_rmse(&fit.map, holdout, world).unwrap(),
                    fit.rank,
                    started.elapsed().as_millis(),
                    fit.map
                        .planets
                        .iter()
                        .map(|p| (p.x, p.y, p.mass))
                        .collect::<Vec<_>>()
                ),
                Err(e) => println!("  recombined_error={e}"),
            }
        }
        let mut online = OnlineEstimator::with_seed_strategy(strategy);
        let mut completed = 0;
        for row in &events {
            let RecordedEvent::Inbound(event) = &row.event else {
                continue;
            };
            if matches!(event, ServerEvent::ShotFin { .. }) {
                completed += 1;
                if completed > train.len() {
                    break;
                }
            }
            online.observe(event, &AtomicBool::new(false));
        }
        if let Some(belief) = online.belief().filter(|b| !b.map.planets.is_empty()) {
            let started = Instant::now();
            let mut initial = belief.map.clone();
            if let Ok(seed) = seed_planet_from_residuals(&initial, train, world, 30.0, 80.0) {
                initial.planets.push(seed);
                let contacts = belief
                    .contact_points
                    .iter()
                    .filter_map(|point| {
                        let near = belief
                            .map
                            .planets
                            .iter()
                            .enumerate()
                            .filter(|(_, p)| {
                                ((f64::from(point.x) - p.x).hypot(f64::from(point.y) - p.y)
                                    - p.radius)
                                    .abs()
                                    <= 8.0
                            })
                            .map(|(i, _)| i)
                            .collect::<Vec<_>>();
                        (near.len() == 1).then(|| ContactConstraint {
                            planet_index: near[0],
                            point: point.clone(),
                        })
                    })
                    .collect::<Vec<_>>();
                match fit_planet_map(&initial, train, &contacts, world, &AtomicBool::new(false)) {
                    Ok(fit) => println!(
                        "  physical_plus_residual count={} train={:.6} holdout={:.6} contacts={} elapsed_ms={} centers={:?}",
                        fit.map.planets.len(),
                        fit.position_rmse,
                        position_rmse(&fit.map, holdout, world).unwrap(),
                        contacts.len(),
                        started.elapsed().as_millis(),
                        fit.map
                            .planets
                            .iter()
                            .map(|p| (p.x, p.y, p.radius, p.mass))
                            .collect::<Vec<_>>()
                    ),
                    Err(e) => println!("  physical_plus_residual_error={e}"),
                }
            }
        }
        if train.len() == 2 {
            for omitted in 0..2 {
                let tr = std::slice::from_ref(&train[1 - omitted]);
                let validation = std::slice::from_ref(&train[omitted]);
                if let Ok(search) = fit_greedy_source_counts(tr, validation, world, 2, 30.0, 80.0) {
                    for c in search.candidates {
                        println!(
                            "  leave_one_out={} count={} train={:.6} validation={:.6}",
                            omitted,
                            c.result.map.planets.len(),
                            c.result.residual_rmse,
                            c.validation_rmse.unwrap()
                        );
                    }
                }
            }
        }
        if id == 12 {
            for omitted in 0..shots.len() {
                let selected = shots
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != omitted)
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>();
                let validation = std::slice::from_ref(&shots[omitted]);
                let started = Instant::now();
                let search =
                    fit_greedy_source_counts(&selected, validation, world, 3, 30.0, 80.0).unwrap();
                for c in search.candidates {
                    println!(
                        "  all_path_fold omitted={} count={} train={:.6} validation={:.6} rank={}/{} total_ms={}",
                        omitted + 1,
                        c.result.map.planets.len(),
                        c.result.residual_rmse,
                        c.validation_rmse.unwrap(),
                        c.result.rank,
                        c.result.map.planets.len() * 3,
                        started.elapsed().as_millis()
                    );
                }
                let mut proposals = Vec::new();
                for shot in &selected {
                    if let Ok(seed) = seed_planet_from_residuals(
                        &flat,
                        std::slice::from_ref(shot),
                        world,
                        30.0,
                        80.0,
                    ) {
                        if let Ok(fit) = fit_fixed_count(
                            &PlanetMap {
                                planets: vec![seed],
                            },
                            std::slice::from_ref(shot),
                            world,
                        ) {
                            proposals.push(fit.map.planets[0].clone());
                        }
                    }
                }
                if proposals.len() == 2 {
                    if let Ok(fit) =
                        fit_fixed_count(&PlanetMap { planets: proposals }, &selected, world)
                    {
                        println!(
                            "  all_path_fold omitted={} recombined=2 train={:.6} validation={:.6} rank={}/6",
                            omitted + 1,
                            fit.residual_rmse,
                            position_rmse(&fit.map, validation, world).unwrap(),
                            fit.rank
                        );
                    }
                }
            }
        }
    }
}
