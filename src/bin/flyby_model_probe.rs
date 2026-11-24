//! Compare first-flyby candidate models on the later captured trial path.
use einsteinbot::estimator::{
    SeedStrategy, fit_greedy_source_counts_with_strategy, position_rmse, shot_observations,
};
use einsteinbot::event_store::EventLog;
use einsteinbot::field_hypothesis::propose_provisional_field_with_strategy;
use einsteinbot::map::PlanetMap;
use einsteinbot::simulation::WorldParameters;
use std::sync::atomic::AtomicBool;
use std::time::Instant;
fn main() {
    let mut args = std::env::args().skip(1);
    let strategy = match (args.next().as_deref(), args.next().as_deref(), args.next()) {
        (None, None, None) | (Some("--seed"), Some("grid"), None) => SeedStrategy::Grid,
        (Some("--seed"), Some("bearing"), None) => SeedStrategy::Bearing,
        _ => panic!("usage: flyby_model_probe [--seed grid|bearing]"),
    };
    let ids = [1, 2, 3, 4, 5, 6, 10, 11, 12, 13, 14, 15, 17];
    let world = WorldParameters::default();
    let flat = PlanetMap { planets: vec![] };
    for id in ids {
        let shots =
            shot_observations(&EventLog::read_capture("learning-scenarios.sqlite3", id).unwrap());
        if shots.len() < 2 {
            continue;
        }
        let training = &shots[..1];
        let heldout = &shots[shots.len() - 1..];
        let flat_error = position_rmse(&flat, heldout, world).unwrap();
        let started = Instant::now();
        let next = &shots[1..2];
        let next_flat = position_rmse(&flat, next, world).unwrap();
        print!("capture={id} fit_seed={strategy:?} flat={flat_error:.6} next_flat={next_flat:.6}");
        match propose_provisional_field_with_strategy(
            &training[0],
            world,
            &AtomicBool::new(false),
            strategy,
        ) {
            None => print!(" provisional=none"),
            Some(p) => {
                let map = PlanetMap {
                    planets: vec![einsteinbot::protocol::Planet {
                        x: p.source.x,
                        y: p.source.y,
                        mass: p.source.mass,
                        radius: 0.0,
                    }],
                };
                print!(
                    " provisional={:.6} provisional_next={:.6} tail={:.6} fit_ms={}",
                    position_rmse(&map, heldout, world).unwrap(),
                    position_rmse(&map, next, world).unwrap(),
                    p.source.continuation_rmse,
                    started.elapsed().as_millis()
                );
            }
        }
        let started = Instant::now();
        match fit_greedy_source_counts_with_strategy(
            training, heldout, world, 2, 30.0, 80.0, strategy,
        ) {
            Ok(search) => {
                for c in search.candidates {
                    print!(
                        " full_path_k{}={:.6}/next{:.6}/train{:.6}",
                        c.result.map.planets.len(),
                        c.validation_rmse.unwrap(),
                        position_rmse(&c.result.map, next, world).unwrap(),
                        c.result.residual_rmse
                    );
                }
            }
            Err(_) => print!(" full_path=unavailable"),
        }
        println!(" full_path_total_ms={}", started.elapsed().as_millis());
    }
}
