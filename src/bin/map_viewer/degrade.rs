//! Degradation experiments: start from perfect information (the disclosed map as the belief),
//! damage it in one controlled way, let the bot's policy choose a shot on the damaged map, then
//! fly that shot on the true map. A hit claimed on the damaged map but missed on the true one is
//! the overconfidence the learned belief has to avoid.
use super::*;
use einsteinbot::simulation::PredictedTrajectory;
use einsteinbot::strategy::{ShotChoice, TargetingPolicy, TargetingSettings};

/// Planets considered for single-planet rungs: the ones whose surface passes closest to the
/// perfect-information shot.
const FOCUS_PLANETS: usize = 5;

fn damaged(planets: &[Planet], index: usize, change: impl FnOnce(&mut Planet)) -> PlanetMap {
    let mut planets = planets.to_vec();
    change(&mut planets[index]);
    PlanetMap { planets }
}

/// Mass follows radius cubed in the game, so resizing keeps the density of the planet.
fn resized(planet: &Planet, radius: f64) -> Planet {
    Planet {
        radius,
        mass: planet.mass * (radius / planet.radius).powi(3),
        ..planet.clone()
    }
}

/// A fixed pseudo-random offset in `[-1, 1]` (no randomness crate: every run is identical).
fn jitter(seed: u64, index: usize, axis: u64) -> f64 {
    let mut x = seed
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add((index as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9))
        .wrapping_add(axis.wrapping_mul(0x94D0_49BB_1331_11EB));
    x ^= x >> 31;
    x = x.wrapping_mul(0xD6E8_FEB8_6659_FD93);
    x ^= x >> 29;
    (x >> 11) as f64 / (1u64 << 52) as f64 - 1.0
}

struct Context {
    truth: PlanetMap,
    state: BotState,
    policy: TargetingPolicy,
    energy: f64,
    own_id: u32,
    start: Point,
    players: Vec<(u32, Vec2)>,
    /// The shot chosen with perfect information; every rung also asks how it fares on the damaged map.
    perfect: Option<ShotChoice>,
}

impl Context {
    fn choose(&self, belief: &PlanetMap) -> Option<ShotChoice> {
        let mut state = self.state.clone();
        state.set_map_belief(Some(MapBelief::new(belief.clone(), None)));
        self.policy.choose(&state, self.energy)
    }

    fn fly(&self, map: &PlanetMap, choice: &ShotChoice) -> Result<PredictedTrajectory, String> {
        let shot = ShotObservation {
            player_id: self.own_id,
            angle_degrees: f64::from(choice.angle_degrees),
            velocity: f64::from(choice.velocity),
            points: vec![self.start.clone()],
        };
        simulate_targeting_shot(
            map,
            &shot,
            WorldParameters::default(),
            TargetingSettings::default().max_simulation_ticks,
            &self.players,
            PLAYER_RADIUS,
        )
    }

    /// Closest approach to the victim of the shot, and whether it ends on that victim.
    fn approach(
        &self,
        map: &PlanetMap,
        choice: &ShotChoice,
    ) -> Result<(Option<f64>, bool), String> {
        let path = self.fly(map, choice)?;
        let victim = choice.predicted_victim;
        let closest = victim.and_then(|victim| {
            let target = self.players.iter().find(|(id, _)| *id == victim)?.1;
            path.points
                .iter()
                .map(|p| (f64::from(p.x) - target.x).hypot(f64::from(p.y) - target.y))
                .min_by(f64::total_cmp)
        });
        let hit = matches!((path.termination, victim), (Termination::PlayerCollision(hit), Some(victim)) if hit == victim);
        Ok((closest, hit))
    }

    /// One rung: the damaged belief's choice, flown on the damaged belief and on the truth.
    fn rung(&self, label: String, params: Value, belief: &PlanetMap) -> Result<Value, String> {
        let Some(choice) = self.choose(belief) else {
            return Ok(json!({"label": label, "params": params, "claimed": null}));
        };
        let believed = self.fly(belief, &choice)?;
        let real = self.fly(&self.truth, &choice)?;
        let victim = choice.predicted_victim;
        let (closest, hit_victim) = self.approach(&self.truth, &choice)?;
        // Sensitivity of the perfect-information shot itself: would it still hit if this damaged
        // map were the real world?
        let fixed = match &self.perfect {
            Some(perfect) => {
                let (closest, hit) = self.approach(belief, perfect)?;
                json!({"closest_to_victim": closest, "hit": hit})
            }
            None => Value::Null,
        };
        Ok(json!({
            "label": label,
            "params": params,
            "claimed": {
                "reason": format!("{:?}", choice.reason),
                "velocity": choice.velocity,
                "angle_degrees": choice.angle_degrees,
                "predicted_hit": choice.predicted_hit,
                "predicted_victim": victim,
                "miss_distance": choice.miss_distance,
            },
            "real": {
                "termination": termination_name(real.termination),
                "hit_victim": hit_victim,
                "closest_to_victim": closest,
            },
            "perfect_shot_on_damaged_map": fixed,
            "claimed_points": json!(&believed.points),
            "real_points": json!(&real.points),
        }))
    }
}

/// `GET /api/degrade?id=&seq=&kind=drop|displace|resize|phantom|noise`
pub fn degrade(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let kind = parameters.get("kind").copied().unwrap_or("drop");
    let connection = open_database(path)?;
    let data = read_decision(&connection, capture_id, sequence)?;
    let events = read_events_before(&connection, capture_id, sequence)?;
    let truth_planets = latest_actual_planets(&events)
        .ok_or("this capture has no disclosed planet map at this decision")?;
    let state = BotState::from_diagnostic_snapshot(&data["bot_state"])
        .map_err(|error| error.to_string())?;
    let own_id = state
        .own_id()
        .ok_or("captured decision has no own player id")?;
    let start = state
        .player_positions()
        .get(&own_id)
        .cloned()
        .ok_or("captured decision has no own player position")?;
    let angle_step = data["angle_step_degrees"]
        .as_u64()
        .unwrap_or(10)
        .clamp(1, 180) as u16;
    let context = Context {
        players: state
            .player_positions()
            .iter()
            .map(|(&id, p)| (id, Vec2::new(f64::from(p.x), f64::from(p.y))))
            .collect(),
        truth: PlanetMap {
            planets: truth_planets.clone(),
        },
        state,
        policy: TargetingPolicy::new(TargetingSettings {
            angle_step_degrees: angle_step,
            ..TargetingSettings::default()
        }),
        energy: data["energy"]
            .as_f64()
            .ok_or("captured energy is missing")?,
        own_id,
        start,
        perfect: None,
    };
    let Some(perfect) = context.choose(&context.truth) else {
        return Err("the policy found no shot even with perfect information".to_owned());
    };
    let context = Context {
        perfect: Some(perfect.clone()),
        ..context
    };
    let baseline = context.rung("Perfect information".to_owned(), json!({}), &context.truth)?;
    // Rank planets by how close the perfect-information shot passes to their surface.
    let path_points = context.fly(&context.truth, &perfect)?.points;
    let mut focus = truth_planets
        .iter()
        .enumerate()
        .filter_map(|(index, planet)| {
            path_points
                .iter()
                .map(|p| {
                    let dx = f64::from(p.x) - planet.x;
                    let dy = f64::from(p.y) - planet.y;
                    (dx.hypot(dy) - planet.radius, (dx, dy))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(gap, offset)| (index, gap, offset))
        })
        .collect::<Vec<_>>();
    focus.sort_by(|a, b| a.1.total_cmp(&b.1));
    focus.truncate(FOCUS_PLANETS);
    let mut rows = vec![baseline];
    match kind {
        "drop" => {
            for &(index, gap, _) in &focus {
                let mut planets = truth_planets.clone();
                planets.remove(index);
                rows.push(context.rung(
                    format!("Drop planet #{index} ({gap:.0} from the path)"),
                    json!({"planet": index, "gap": gap}),
                    &PlanetMap { planets },
                )?);
            }
        }
        "displace" | "resize" => {
            let &(index, gap, (dx, dy)) = focus.first().ok_or("no planets to degrade")?;
            let norm = dx.hypot(dy).max(1e-9);
            let planet = &truth_planets[index];
            let ladder: &[f64] = if kind == "displace" {
                &[5.0, 10.0, 20.0, 40.0]
            } else {
                &[-6.0, -3.0, 3.0, 6.0]
            };
            for &amount in ladder {
                let (label, belief) = if kind == "displace" {
                    (
                        format!("Move planet #{index} {amount:.0} units away from the path"),
                        damaged(&truth_planets, index, |p| {
                            p.x += dx / norm * amount;
                            p.y += dy / norm * amount;
                        }),
                    )
                } else {
                    (
                        format!("Planet #{index} radius {amount:+.0} (mass follows)"),
                        damaged(&truth_planets, index, |p| {
                            *p = resized(planet, (planet.radius + amount).max(5.0));
                        }),
                    )
                };
                rows.push(context.rung(
                    label,
                    json!({"planet": index, "gap": gap, "amount": amount}),
                    &belief,
                )?);
            }
        }
        "phantom" => {
            let middle = &path_points[path_points.len() / 2];
            let density = median_density(&truth_planets);
            for distance in [100.0, 200.0, 400.0] {
                let mut planets = truth_planets.clone();
                planets.push(Planet {
                    x: f64::from(middle.x),
                    y: f64::from(middle.y) + distance,
                    radius: 30.0,
                    mass: density * 30.0_f64.powi(3),
                });
                rows.push(context.rung(
                    format!("Add a phantom planet {distance:.0} below the path"),
                    json!({"distance": distance}),
                    &PlanetMap { planets },
                )?);
            }
        }
        "noise" => {
            for sigma in [2.0, 5.0, 10.0] {
                for seed in 1..=2u64 {
                    let planets = truth_planets
                        .iter()
                        .enumerate()
                        .map(|(i, p)| Planet {
                            x: p.x + jitter(seed, i, 0) * sigma * 3.0_f64.sqrt(),
                            y: p.y + jitter(seed, i, 1) * sigma * 3.0_f64.sqrt(),
                            ..p.clone()
                        })
                        .collect();
                    rows.push(context.rung(
                        format!("Every planet off by ~{sigma:.0} units (draw {seed})"),
                        json!({"sigma": sigma, "seed": seed}),
                        &PlanetMap { planets },
                    )?);
                }
            }
        }
        _ => return Err("kind must be drop, displace, resize, phantom or noise".to_owned()),
    }
    Ok(json!({
        "kind": kind,
        "rows": rows,
        "note": "Starts from the disclosed map as the belief, applies one degradation, lets the current policy choose a shot on the damaged map, and flies it on the true map with the decision's player positions. Policy as of the current code, not the one that recorded this decision.",
    }))
}

/// Typical mass per radius cubed of the real planets, so invented planets look like real ones.
fn median_density(planets: &[Planet]) -> f64 {
    let mut densities = planets
        .iter()
        .map(|p| p.mass / p.radius.powi(3))
        .collect::<Vec<_>>();
    densities.sort_by(f64::total_cmp);
    densities.get(densities.len() / 2).copied().unwrap_or(0.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_deterministic_and_bounded() {
        for index in 0..50 {
            let value = jitter(1, index, 0);
            assert!((-1.0..=1.0).contains(&value));
            assert_eq!(value, jitter(1, index, 0));
        }
        assert_ne!(jitter(1, 3, 0), jitter(2, 3, 0));
    }

    #[test]
    fn resizing_keeps_density() {
        let planet = Planet {
            x: 0.0,
            y: 0.0,
            radius: 20.0,
            mass: 800.0,
        };
        let bigger = resized(&planet, 30.0);
        assert!((bigger.mass / bigger.radius.powi(3) - 0.1).abs() < 1e-12);
    }
}
