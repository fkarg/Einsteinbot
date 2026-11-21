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
    // splitmix64 over the combined key.
    let mut x = seed
        .wrapping_mul(0x1000_0000_0000_0001)
        .wrapping_add((index as u64) << 8 | axis)
        .wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^= x >> 31;
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
}

/// Closest approach (center to center) of a path to a player; the target radius is 4.
fn closest_approach(path: &PredictedTrajectory, target: Option<Vec2>) -> Option<f64> {
    let target = target?;
    path.points
        .iter()
        .map(|p| (f64::from(p.x) - target.x).hypot(f64::from(p.y) - target.y))
        .min_by(f64::total_cmp)
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

    fn position_of(&self, victim: Option<u32>) -> Option<Vec2> {
        self.players
            .iter()
            .find(|(id, _)| Some(*id) == victim)
            .map(|(_, p)| *p)
    }

    /// One rung: the damaged belief's choice, flown on the damaged belief and on the truth, and
    /// the perfect-information shot flown on the damaged belief. One failing rung does not stop
    /// the others.
    fn rung(&self, label: String, belief: &PlanetMap, perfect: &ShotChoice) -> Value {
        self.try_rung(&label, belief, perfect)
            .unwrap_or_else(|error| json!({"label": label, "error": error}))
    }

    fn try_rung(
        &self,
        label: &str,
        belief: &PlanetMap,
        perfect: &ShotChoice,
    ) -> Result<Value, String> {
        // The perfect shot on the damaged map: would it still hit if this were the real world?
        let fixed = self.fly(belief, perfect)?;
        let fixed_hit = matches!(
            (fixed.termination, perfect.predicted_victim),
            (Termination::PlayerCollision(hit), Some(victim)) if hit == victim
        );
        let fixed = json!({
            "hit": fixed_hit,
            "closest_approach": closest_approach(&fixed, self.position_of(perfect.predicted_victim)),
        });
        let Some(choice) = self.choose(belief) else {
            return Ok(
                json!({"label": label, "claimed": null, "perfect_shot_on_damaged_map": fixed}),
            );
        };
        let believed = self.fly(belief, &choice)?;
        let real = self.fly(&self.truth, &choice)?;
        let hit_player = match real.termination {
            Termination::PlayerCollision(id) => Some(id),
            _ => None,
        };
        Ok(json!({
            "label": label,
            "claimed": choice_json(&choice),
            "real": {
                "termination": termination_name(real.termination),
                "hit_player": hit_player,
                "hit_intended": hit_player.is_some() && hit_player == choice.predicted_victim,
                "closest_approach": closest_approach(&real, self.position_of(choice.predicted_victim)),
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
    if !["drop", "displace", "resize", "phantom", "noise"].contains(&kind) {
        return Err("kind must be drop, displace, resize, phantom or noise".to_owned());
    }
    let connection = open_database(path)?;
    let data = read_decision(&connection, capture_id, sequence)?;
    let events = read_events_before(&connection, capture_id, sequence)?;
    let truth_planets = latest_actual_planets(&events)
        .ok_or("this capture has no disclosed planet map at this decision")?;
    if truth_planets.is_empty() {
        return Err("the disclosed map has no planets to damage".to_owned());
    }
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
    // A recorded decision without a step falls back to the policy's usual 10 degrees.
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
    };
    let perfect = context
        .choose(&context.truth)
        .filter(|choice| choice.predicted_hit && choice.predicted_victim.is_some())
        .ok_or("with perfect information the policy finds no hit at this decision, so there is no shot to degrade; pick a decision where the disclosed-map policy hits")?;
    let mut rows = vec![context.rung("Perfect information".to_owned(), &context.truth, &perfect)];
    // Rank planets by how close their surface passes to the perfect-information shot.
    let path_points = context.fly(&context.truth, &perfect)?.points;
    let mut focus = truth_planets
        .iter()
        .enumerate()
        .filter_map(|(index, planet)| {
            path_points
                .iter()
                .map(|p| {
                    let (dx, dy) = (planet.x - f64::from(p.x), planet.y - f64::from(p.y));
                    (dx.hypot(dy) - planet.radius, (dx, dy))
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(gap, offset)| (index, gap.max(0.0), offset))
        })
        .collect::<Vec<_>>();
    focus.sort_by(|a, b| a.1.total_cmp(&b.1));
    focus.truncate(FOCUS_PLANETS);
    match kind {
        "drop" => {
            for &(index, gap, _) in &focus {
                let mut planets = truth_planets.clone();
                planets.remove(index);
                rows.push(context.rung(
                    format!("Drop planet #{index} (its surface is {gap:.0} from the path)"),
                    &PlanetMap { planets },
                    &perfect,
                ));
            }
        }
        "displace" => {
            let &(index, _, (dx, dy)) = &focus[0];
            // The offset points from the path to the planet: moving along it moves the planet away.
            let norm = dx.hypot(dy).max(1e-9);
            for amount in [1.0, 2.0, 5.0, 10.0, 20.0] {
                rows.push(context.rung(
                    format!("Move planet #{index} {amount:.0} units away from the path"),
                    &damaged(&truth_planets, index, |p| {
                        p.x += dx / norm * amount;
                        p.y += dy / norm * amount;
                    }),
                    &perfect,
                ));
            }
        }
        "resize" => {
            let &(index, _, _) = &focus[0];
            let planet = &truth_planets[index];
            for delta in [-6.0, -3.0, -1.0, 1.0, 3.0, 6.0] {
                let radius = (planet.radius + delta).max(5.0);
                rows.push(context.rung(
                    format!(
                        "Planet #{index} radius {:+.0} (mass follows)",
                        radius - planet.radius
                    ),
                    &damaged(&truth_planets, index, |p| *p = resized(planet, radius)),
                    &perfect,
                ));
            }
        }
        "phantom" => {
            let middle = &path_points[path_points.len() / 2];
            let density = median_density(&truth_planets);
            let world = WorldParameters::default();
            for distance in [100.0, 200.0, 400.0] {
                // Try below, then above the path; keep it on the field and clear of everything.
                let candidate = [distance, -distance].into_iter().find_map(|offset| {
                    let (x, y) = (f64::from(middle.x), f64::from(middle.y) + offset);
                    let clear = (30.0..=world.battlefield_width - 30.0).contains(&x)
                        && (30.0..=world.battlefield_height - 30.0).contains(&y)
                        && truth_planets
                            .iter()
                            .all(|p| (p.x - x).hypot(p.y - y) > p.radius + 60.0)
                        && context
                            .players
                            .iter()
                            .all(|(_, q)| (q.x - x).hypot(q.y - y) > 60.0);
                    clear.then_some((x, y))
                });
                match candidate {
                    Some((x, y)) => {
                        let mut planets = truth_planets.clone();
                        planets.push(Planet { x, y, radius: 30.0, mass: density * 30.0_f64.powi(3) });
                        rows.push(context.rung(
                            format!("Add a phantom planet {distance:.0} units from the path"),
                            &PlanetMap { planets },
                            &perfect,
                        ));
                    }
                    None => rows.push(json!({"label": format!("Phantom planet {distance:.0} units from the path"), "error": "no position on the field clear of planets and players"})),
                }
            }
        }
        _ => {
            for sigma in [1.0, 2.0, 5.0] {
                for seed in 1..=3u64 {
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
                        format!("Every planet off by ~{sigma:.0} units per axis (draw {seed})"),
                        &PlanetMap { planets },
                        &perfect,
                    ));
                }
            }
        }
    }
    Ok(json!({
        "kind": kind,
        "rows": rows,
        "note": "Starts from the disclosed map as the belief, applies one degradation, lets the current policy choose a shot on the damaged map (with the recorded shot history and coverage), and flies it on the true map with the decision's player positions. Policy as of the current code, not the one that recorded this decision. Rungs choose different shots, so compare the last-but-one column across rungs for a like-for-like sensitivity.",
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
