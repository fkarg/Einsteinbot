use super::{Launch, Scenario, planet};
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::Planet;
use einsteinbot::simulation::{WorldParameters, simulate_shot};
use rand::RngExt;
use rand::SeedableRng;
use rand::rngs::StdRng;

pub(super) const SEED: u64 = 20_261_121;
const SHOT_BUDGET: usize = 8;

/// One deterministic game-scale fixture. Kept deliberately small until batched fitting exists.
pub(super) fn scenario() -> Scenario {
    let mut rng = StdRng::seed_from_u64(SEED);
    let world = WorldParameters::default();
    let mut planets = Vec::<Planet>::with_capacity(24);
    while planets.len() < 24 {
        let radius = rng.random_range(20.0..40.0);
        let candidate = planet(
            rng.random_range(radius..world.battlefield_width - radius),
            rng.random_range(radius..world.battlefield_height - radius),
            radius,
        );
        if planets.iter().all(|existing| {
            (existing.x - candidate.x).hypot(existing.y - candidate.y)
                > existing.radius + candidate.radius
        }) {
            planets.push(candidate);
        }
    }
    let truth = PlanetMap { planets };
    let mut observations = Vec::with_capacity(SHOT_BUDGET);
    let mut trial = None;
    let mut target_tick = 0;
    for _ in 0..(SHOT_BUDGET * 20) {
        let launch = Launch {
            x: rng.random_range(100.0..(world.battlefield_width as f32 - 100.0)),
            y: rng.random_range(100.0..(world.battlefield_height as f32 - 100.0)),
            angle: rng.random_range(0..360),
            speed: 8,
        };
        let path = simulate_shot(&truth, &super::shot(launch), world, 2000)
            .expect("generated W7 shot is simulatable");
        if path.points.len() < 40 {
            continue;
        }
        observations.push((launch, path.termination));
        if trial.is_none() {
            trial = Some(launch);
            target_tick = path.points.len() / 2;
        }
        if observations.len() == SHOT_BUDGET {
            break;
        }
    }
    assert_eq!(
        observations.len(),
        SHOT_BUDGET,
        "seeded W7 fixture did not produce enough long paths"
    );
    let trial = trial.expect("seeded W7 fixture includes a sufficiently long path");
    Scenario {
        name: "w7_scale_24_seed_20261121",
        family: "W7",
        defect_class: "scale_24",
        truth_intended_planet_indices: &[],
        truth,
        observations,
        trial,
        target_tick,
    }
}
