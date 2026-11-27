//! `devtool degrade`: degradation ladders D1-D6 (scenario-wishlist §7). The belief is the true
//! map damaged in one controlled way; the policy chooses on it and the shot flies on the true
//! map with the decision's players. The batch counterpart of the inspector's per-decision
//! degrade view: which kind and size of map error costs hits, and how much the belief overclaims
//! (predicted-hit rate minus true-hit rate).
use crate::config::{Config, fnv1a};
use crate::targeting::{Outcome, par_map, score_once, situations};
use crate::world::{self, World};
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, Point};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::{BotState, MapBelief, TargetingPolicy};
use serde_json::json;
use std::collections::BTreeMap;

/// One situation to degrade: the bot state (positions, history, own id) and energy.
struct Case {
    state: BotState,
    energy: f64,
    truth: Vec<Planet>,
    seed: u64,
}

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// A fixed pseudo-random value in [-1, 1).
fn jitter(seed: u64, key: u64) -> f64 {
    (splitmix(seed ^ splitmix(key)) >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

fn resized(p: &Planet, radius: f64) -> Planet {
    Planet {
        radius,
        mass: p.mass * (radius / p.radius).powi(3),
        ..p.clone()
    }
}

/// Planets ordered by how close their surface passes to `path` (closest first).
fn by_gap(planets: &[Planet], path: &[Point]) -> Vec<usize> {
    let gap = |p: &Planet| {
        path.iter()
            .map(|q| (p.x - f64::from(q.x)).hypot(p.y - f64::from(q.y)) - p.radius)
            .fold(f64::INFINITY, f64::min)
    };
    let mut order: Vec<usize> = (0..planets.len()).collect();
    order.sort_by(|&a, &b| gap(&planets[a]).total_cmp(&gap(&planets[b])));
    order
}

/// The rungs of every ladder for one case: (ladder, rung label, belief map, moved players).
fn rungs(case: &Case, path: &[Point]) -> Vec<(&'static str, String, Vec<Planet>, Option<f64>)> {
    let t = &case.truth;
    let order = by_gap(t, path);
    let near = order[0];
    let mut out = Vec::new();
    let without = |drop: &[usize]| -> Vec<Planet> {
        t.iter()
            .enumerate()
            .filter(|(i, _)| !drop.contains(i))
            .map(|(_, p)| p.clone())
            .collect()
    };
    // D1: drop planets.
    out.push(("D1_drop_planet", "nearest".into(), without(&[near]), None));
    out.push((
        "D1_drop_planet",
        "2 nearest".into(),
        without(&order[..2.min(order.len())]),
        None,
    ));
    let largest = (0..t.len())
        .max_by(|&a, &b| t[a].radius.total_cmp(&t[b].radius))
        .unwrap();
    out.push((
        "D1_drop_planet",
        "largest".into(),
        without(&[largest]),
        None,
    ));
    let random = (splitmix(case.seed) % t.len() as u64) as usize;
    out.push(("D1_drop_planet", "random".into(), without(&[random]), None));
    let far = *order.last().unwrap();
    out.push(("D1_drop_planet", "farthest".into(), without(&[far]), None));
    // D2: displace the nearest planet in a fixed random direction.
    let angle = std::f64::consts::PI * jitter(case.seed, 2);
    for d in [2.0, 5.0, 10.0, 20.0, 40.0] {
        let mut m = t.clone();
        m[near].x += d * angle.cos();
        m[near].y += d * angle.sin();
        out.push(("D2_displace_planet", format!("{d}"), m, None));
    }
    // D3: resize the nearest planet (mass follows radius cubed).
    for d in [-10.0, -5.0, -2.0, 2.0, 5.0, 10.0] {
        let mut m = t.clone();
        m[near] = resized(&t[near], (t[near].radius + d).max(5.0));
        out.push(("D3_resize_planet", format!("{d:+}"), m, None));
    }
    // D4: a phantom planet (radius 30) beside the middle of the path, clear of the others.
    let w = WorldParameters::default();
    let mid = &path[path.len() / 2];
    for d in [50.0, 100.0, 200.0] {
        let place = [0.0, 0.5, 1.0, 1.5].iter().find_map(|turn| {
            let a = angle + turn * std::f64::consts::PI;
            let (x, y) = (
                f64::from(mid.x) + d * a.cos(),
                f64::from(mid.y) + d * a.sin(),
            );
            let clear = (30.0..=w.battlefield_width - 30.0).contains(&x)
                && (30.0..=w.battlefield_height - 30.0).contains(&y)
                && t.iter().all(|p| (p.x - x).hypot(p.y - y) > p.radius + 40.0)
                && case
                    .state
                    .player_positions()
                    .values()
                    .all(|q| (f64::from(q.x) - x).hypot(f64::from(q.y) - y) > 45.0);
            clear.then_some((x, y))
        });
        if let Some((x, y)) = place {
            let mut m = t.clone();
            m.push(Planet {
                x,
                y,
                radius: 30.0,
                mass: 2700.0,
            });
            out.push(("D4_phantom_planet", format!("{d}"), m, None));
        }
    }
    // D5: the targets moved after the decision (the policy sees the old positions).
    for d in [5.0, 10.0, 20.0, 40.0] {
        out.push(("D5_stale_player", format!("{d}"), t.clone(), Some(d)));
    }
    // D6: every planet's centre and radius jittered with sd sigma (uniform draws).
    for sigma in [1.0, 2.0, 5.0, 10.0] {
        let s3 = sigma * 3f64.sqrt();
        let m = t
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let k = 8 * i as u64;
                let moved = Planet {
                    x: p.x + s3 * jitter(case.seed, k + 1),
                    y: p.y + s3 * jitter(case.seed, k + 2),
                    ..p.clone()
                };
                resized(
                    &moved,
                    (p.radius + 0.5 * s3 * jitter(case.seed, k + 3)).max(5.0),
                )
            })
            .collect();
        out.push(("D6_noisy_map", format!("{sigma}"), m, None));
    }
    out
}

/// Move every opponent `d` units in a fixed random direction (D5: the truth the shot meets).
fn moved_state(state: &BotState, d: f64, seed: u64) -> BotState {
    let mut moved = state.clone();
    let own = state.own_id();
    for (id, p) in state.player_positions() {
        if Some(*id) == own {
            continue;
        }
        let a = std::f64::consts::PI * jitter(seed, 1000 + u64::from(*id));
        moved.set_player_position(
            *id,
            Point {
                x: p.x + (d * a.cos()) as f32,
                y: p.y + (d * a.sin()) as f32,
            },
        );
    }
    moved
}

fn cases(world: &World, paths: usize, count: usize) -> Vec<Case> {
    let seed = fnv1a(world.id.as_bytes());
    if world.is_capture() {
        let n = world.decisions.len();
        let step = (n / count.max(1)).max(1);
        world
            .decisions
            .iter()
            .step_by(step)
            .take(count)
            .enumerate()
            .filter_map(|(i, d)| {
                Some(Case {
                    state: BotState::from_diagnostic_snapshot(&d.data["bot_state"]).ok()?,
                    energy: d.data["energy"].as_f64()?,
                    truth: world.truth.clone(),
                    seed: seed ^ i as u64,
                })
            })
            .collect()
    } else {
        let history: Vec<ShotObservation> = world
            .paths()
            .into_iter()
            .take(paths)
            .map(|(_, o)| o)
            .collect();
        situations(seed, &world.truth, count)
            .into_iter()
            .enumerate()
            .map(|(i, s)| Case {
                state: s.state(&history),
                energy: s.energy,
                truth: world.truth.clone(),
                seed: seed ^ i as u64,
            })
            .collect()
    }
}

#[derive(Default)]
struct Tally {
    n: usize,
    hits: usize,
    predicted: usize,
    lost: usize,
    perfect_hits: usize,
    misses: Vec<f64>,
}

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut specs, mut config_path, mut db) =
        (String::new(), None, "einsteinbot.sqlite3".to_owned());
    let (mut paths, mut count, mut threads) = (32usize, 16usize, 1usize);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        let num = |v: String| v.parse::<usize>().map_err(|e| format!("{e}"));
        match arg.as_str() {
            "--worlds" => specs = value()?,
            "--config" => config_path = Some(value()?),
            "--db" => db = value()?,
            "--paths" => paths = num(value()?)?,
            "--situations" => count = num(value()?)?,
            "--threads" => threads = num(value()?)?.max(1),
            _ => {
                println!(
                    "Usage: devtool degrade --worlds SPEC,SPEC,... [--situations N (16; recorded decisions for captures, evenly spaced)] [--paths N (32: synthetic shot history)] [--config cfg.json] [--threads N] [--db PATH]\nOne JSON line per world and rung, then one summary line per rung over all worlds. A case counts when the policy fires on the perfect map."
                );
                return Ok(());
            }
        }
    }
    let config = Config::load(config_path.as_deref())?;
    let policy = TargetingPolicy::new(config.targeting);
    let mut total: BTreeMap<(String, String), Tally> = BTreeMap::new();
    for spec in specs.split(',').filter(|s| !s.is_empty()) {
        let world = world::load(spec, paths, &db)?;
        let all = cases(&world, paths, count);
        let per_case = par_map(all.len(), threads, |i| {
            let case = &all[i];
            let truth = PlanetMap {
                planets: case.truth.clone(),
            };
            let on = |planets: &[Planet], state: &BotState, meets: &PlanetMap| -> Outcome {
                let mut st = state.clone();
                st.set_map_belief(Some(MapBelief::new(
                    PlanetMap {
                        planets: planets.to_vec(),
                    },
                    None,
                )));
                score_once(&policy, meets, &st, case.energy)
            };
            let perfect = on(&case.truth, &case.state, &truth);
            if !perfect.fired {
                return None;
            }
            // The perfect shot's true path, for picking the planets near it.
            let mut st = case.state.clone();
            st.set_map_belief(Some(MapBelief::new(truth.clone(), None)));
            let path = policy
                .choose(&st, case.energy)
                .and_then(|c| {
                    let own = st.own_id()?;
                    let launch = st.player_positions().get(&own)?.clone();
                    einsteinbot::simulation::simulate_shot(
                        &truth,
                        &ShotObservation {
                            player_id: own,
                            angle_degrees: f64::from(c.angle_degrees),
                            velocity: f64::from(c.velocity),
                            points: vec![launch],
                        },
                        WorldParameters::default(),
                        2000,
                    )
                    .ok()
                })
                .map(|p| p.points)?;
            let mut rows = vec![("D0_perfect", "truth".to_owned(), perfect.clone())];
            for (ladder, label, map, moved) in rungs(case, &path) {
                let o = match moved {
                    // D5: choose on the truth with the old positions; the shot meets moved players.
                    Some(d) => {
                        let mut st = case.state.clone();
                        st.set_map_belief(Some(MapBelief::new(truth.clone(), None)));
                        let choice = policy.choose(&st, case.energy);
                        let mv = moved_state(&case.state, d, case.seed);
                        crate::targeting::fly_choice(&truth, &mv, choice)
                    }
                    None => on(&map, &case.state, &truth),
                };
                rows.push((ladder, label, o));
            }
            Some((perfect.hit, rows))
        });
        let mut per_world: BTreeMap<(String, String), Tally> = BTreeMap::new();
        for (perfect_hit, rows) in per_case.into_iter().flatten() {
            for (ladder, label, o) in rows {
                for tally in [
                    per_world.entry((ladder.into(), label.clone())).or_default(),
                    total.entry((ladder.into(), label.clone())).or_default(),
                ] {
                    tally.n += 1;
                    tally.hits += usize::from(o.fired && o.hit);
                    tally.predicted += usize::from(o.fired && o.predicted_hit);
                    tally.perfect_hits += usize::from(perfect_hit);
                    tally.lost += usize::from(perfect_hit && !(o.fired && o.hit));
                    if o.fired
                        && !o.hit
                        && let Some(m) = o.miss_distance
                    {
                        tally.misses.push(m);
                    }
                }
            }
        }
        for ((ladder, label), t) in &per_world {
            println!("{}", line(Some(&world.id), ladder, label, t));
        }
    }
    for ((ladder, label), t) in &total {
        println!("{}", line(None, ladder, label, t));
    }
    Ok(())
}

fn line(world: Option<&str>, ladder: &str, label: &str, t: &Tally) -> serde_json::Value {
    let mut m = t.misses.clone();
    m.sort_by(f64::total_cmp);
    let n = t.n.max(1) as f64;
    json!({
        "world": world.unwrap_or("all"), "ladder": ladder, "rung": label, "cases": t.n,
        "hit_rate": t.hits as f64 / n, "predicted_hit_rate": t.predicted as f64 / n,
        "overconfidence_gap": (t.predicted as f64 - t.hits as f64) / n,
        "lost_perfect_hits": t.lost, "perfect_hits": t.perfect_hits,
        "median_miss_distance": m.get(m.len() / 2),
    })
}
