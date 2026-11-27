//! Targeting scored on the true map (the P15/P17/P20 replay method): the policy chooses a shot
//! on a belief, the shot flies on the truth with the decision's player positions, and only a
//! real player collision counts as a hit. Truth never reaches the policy.
use crate::world::{Decision, World};
use einsteinbot::estimator::{ResidualModel, residual_correction};
use einsteinbot::in_flight::FIRE_LATENCY_MS;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_targeting_shot,
};
use einsteinbot::strategy::{BotState, MapBelief, ShotChoice, TargetingPolicy, TargetingSettings};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Instant;

pub const PLAYER_RADIUS: f64 = 4.0;
/// Energy gained per second of waiting in the replays (the server's rate, P20).
const ENERGY_RATE: f64 = 2.55;
/// Energy at which a waiting bot stops waiting (the replays' cap, P20).
const ENERGY_FULL: f64 = 52.0;

/// One chosen shot flown on the truth.
#[derive(Clone, Debug, Default)]
pub struct Outcome {
    pub fired: bool,
    pub predicted_hit: bool,
    pub hit: bool,
    /// A true hit whose victim the recorded game killed (repositioned) before our impact.
    pub wasted: bool,
    pub velocity: f64,
    /// Closest approach of the true path to the predicted victim (or the nearest opponent).
    pub miss_distance: Option<f64>,
    pub wait_s: f64,
    pub choose_ms: f64,
}

/// A decision situation without a recording (synthetic worlds and degradation ladders).
#[derive(Clone, Debug)]
pub struct Situation {
    pub own: u32,
    pub players: Vec<(u32, Point)>,
    pub energy: f64,
}

impl Situation {
    pub fn state(&self, history: &[ShotObservation]) -> BotState {
        let mut state = BotState::default();
        state.observe(&ServerEvent::OwnId {
            player_id: self.own,
        });
        for (id, p) in &self.players {
            state.observe(&ServerEvent::PlayerPosition {
                player_id: *id,
                x: p.x,
                y: p.y,
            });
        }
        for obs in history {
            state.observe(&ServerEvent::ShotFin {
                player_id: obs.player_id,
                angle: obs.angle_degrees,
                velocity: obs.velocity,
                points: obs.points.clone(),
            });
        }
        state
    }
}

/// `count` deterministic situations on a map: a shooter and three opponents clear of the
/// planets and of each other, energy 10-52.
pub fn situations(seed: u64, truth: &[Planet], count: usize) -> Vec<Situation> {
    let world = WorldParameters::default();
    let mut state = seed ^ 0x7461_7267_6574_7321;
    let mut uniform = || {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..count)
        .map(|_| {
            let mut players: Vec<(u32, Point)> = Vec::new();
            while players.len() < 4 {
                let p = Point {
                    x: (60.0 + (world.battlefield_width - 120.0) * uniform()) as f32,
                    y: (60.0 + (world.battlefield_height - 120.0) * uniform()) as f32,
                };
                let clear = truth
                    .iter()
                    .all(|t| (t.x - f64::from(p.x)).hypot(t.y - f64::from(p.y)) > t.radius + 15.0)
                    && players
                        .iter()
                        .all(|(_, q)| (q.x - p.x).hypot(q.y - p.y) > 80.0);
                if clear {
                    players.push((players.len() as u32 + 1, p));
                }
            }
            Situation {
                own: 1,
                players,
                energy: 10.0 + 42.0 * uniform(),
            }
        })
        .collect()
}

fn as_vec2(players: &BTreeLike) -> Vec<(u32, Vec2)> {
    players
        .iter()
        .map(|(id, p)| (*id, Vec2::new(f64::from(p.x), f64::from(p.y))))
        .collect()
}
type BTreeLike = std::collections::BTreeMap<u32, Point>;

/// Fly `choice`'s shot from the own position on the truth.
fn fly(
    truth: &PlanetMap,
    state: &BotState,
    own: u32,
    velocity: i32,
    angle: i32,
) -> Option<(
    einsteinbot::simulation::PredictedTrajectory,
    Vec<(u32, Vec2)>,
)> {
    let launch = state.player_positions().get(&own).cloned()?;
    let players = as_vec2(state.player_positions());
    let shot = ShotObservation {
        player_id: own,
        angle_degrees: f64::from(angle),
        velocity: f64::from(velocity),
        points: vec![launch],
    };
    let path = simulate_targeting_shot(
        truth,
        &shot,
        WorldParameters::default(),
        2000,
        &players,
        PLAYER_RADIUS,
    )
    .ok()?;
    Some((path, players))
}

/// Choose on the state's belief at `energy` and fly the shot on the truth (no waiting).
pub fn score_once(
    policy: &TargetingPolicy,
    truth: &PlanetMap,
    state: &BotState,
    energy: f64,
) -> Outcome {
    let started = Instant::now();
    let choice = policy.choose(state, energy);
    let ms = started.elapsed().as_secs_f64() * 1e3;
    Outcome {
        choose_ms: ms,
        ..fly_choice(truth, state, choice)
    }
}

/// Fly a chosen shot from the state's own position on the truth, against the state's players.
pub fn fly_choice(truth: &PlanetMap, state: &BotState, choice: Option<ShotChoice>) -> Outcome {
    let mut o = Outcome::default();
    let (Some(c), Some(own)) = (choice, state.own_id()) else {
        return o;
    };
    let Some((path, players)) = fly(truth, state, own, c.velocity, c.angle_degrees) else {
        return o;
    };
    o.fired = true;
    o.predicted_hit = c.predicted_hit;
    o.velocity = f64::from(c.velocity);
    o.hit = matches!(path.termination, Termination::PlayerCollision(p) if p != own);
    o.miss_distance = miss_distance(&path.points, &players, own, c.predicted_victim, o.hit);
    o
}

fn miss_distance(
    points: &[Point],
    players: &[(u32, Vec2)],
    own: u32,
    victim: Option<u32>,
    hit: bool,
) -> Option<f64> {
    if hit {
        return Some(0.0);
    }
    let targets: Vec<&Vec2> = players
        .iter()
        .filter(|(id, _)| *id != own && victim.is_none_or(|v| v == *id))
        .map(|(_, p)| p)
        .collect();
    targets
        .iter()
        .flat_map(|t| {
            points
                .iter()
                .skip(1)
                .map(move |p| (f64::from(p.x) - t.x).hypot(f64::from(p.y) - t.y))
        })
        .min_by(f64::total_cmp)
}

/// Totals of a set of outcomes.
pub fn summary(outcomes: &[Outcome]) -> Value {
    let fired: Vec<&Outcome> = outcomes.iter().filter(|o| o.fired).collect();
    let hits = fired.iter().filter(|o| o.hit && !o.wasted).count();
    let wasted = fired.iter().filter(|o| o.wasted).count();
    let predicted = fired.iter().filter(|o| o.predicted_hit).count();
    let energy: f64 = fired.iter().map(|o| o.velocity).sum();
    let mut misses: Vec<f64> = fired
        .iter()
        .filter(|o| !o.hit)
        .filter_map(|o| o.miss_distance)
        .collect();
    misses.sort_by(f64::total_cmp);
    let mut ms: Vec<f64> = outcomes.iter().map(|o| o.choose_ms).collect();
    ms.sort_by(f64::total_cmp);
    let q = |v: &[f64], q: f64| {
        (!v.is_empty()).then(|| v[((v.len() as f64 - 1.0) * q).round() as usize])
    };
    let n = outcomes.len().max(1) as f64;
    json!({
        "decisions": outcomes.len(),
        "fired": fired.len(),
        "true_hits": hits,
        "wasted": wasted,
        "predicted_hits": predicted,
        "energy": energy,
        "hits_per_100_energy": 100.0 * hits as f64 / energy.max(1.0),
        "hit_rate": hits as f64 / n,
        "predicted_hit_rate": predicted as f64 / n,
        // Predicted-hit rate minus true-hit rate: how much the belief overclaims.
        "overconfidence_gap": (predicted as f64 - hits as f64) / n,
        "median_miss_distance": q(&misses, 0.5),
        "wait_s": fired.iter().map(|o| o.wait_s).sum::<f64>(),
        "choose_ms_p50": q(&ms, 0.5),
        "choose_ms_p95": q(&ms, 0.95),
    })
}

/// Per-decision belief of a capture replay: the planets and the model scalars (the residual
/// correction is rebuilt from them and the map's paths before the decision, as recorded beliefs
/// carry no rows).
#[derive(Clone, Debug)]
pub struct BeliefRecord {
    pub planets: Vec<Planet>,
    pub model: Option<ResidualModel>,
}

impl BeliefRecord {
    pub fn recorded(data: &Value) -> Self {
        Self {
            planets: serde_json::from_value(data["map_belief"]["planets"].clone())
                .unwrap_or_default(),
            model: serde_json::from_value(data["map_belief"]["residual_model"].clone()).ok(),
        }
    }

    pub fn belief(&self, paths: &[ShotObservation]) -> MapBelief {
        let map = PlanetMap {
            planets: self.planets.clone(),
        };
        let (_, built) = residual_correction(&map, paths, self.model);
        let mut belief = MapBelief::new(map, None);
        belief.residual_correction = built.ok();
        belief
    }
}

/// Replay a capture map's recorded decisions with `settings` (P20 method). `beliefs[i]` is the
/// belief of decision `i`. Bot state comes from the recorded events; while the policy waits the
/// replay advances with the recorded game (one-second steps, energy +2.55/s, until 52). A true
/// hit whose victim the recorded game repositions before our impact by someone else's shot is
/// wasted.
pub fn replay_capture(
    world: &World,
    settings: TargetingSettings,
    beliefs: &[Option<BeliefRecord>],
    threads: usize,
) -> Vec<Outcome> {
    let events = &world.events;
    let decisions = &world.decisions;
    let truth = PlanetMap {
        planets: world.truth.clone(),
    };
    // Repositions per player: (time, killer shot (shooter, begin time) when attributable).
    let mut repos: HashMap<u32, Vec<(i64, Option<(u32, i64)>)>> = HashMap::new();
    let mut last_pos: HashMap<u32, (f32, f32)> = HashMap::new();
    let mut begins: Vec<(u32, f64, f64, i64)> = vec![];
    let mut fins: Vec<(i64, i64, ShotObservation)> = vec![];
    for (i, e) in events.iter().enumerate() {
        match &e.event {
            // Positions before the map keep `last_pos` current, so the first in-map reposition
            // can be attributed too.
            ServerEvent::PlayerPosition { player_id, x, y } if i < world.start => {
                last_pos.insert(*player_id, (*x, *y));
            }
            _ if i < world.start => {}
            ServerEvent::PlayerPosition { player_id, x, y } => {
                let old = last_pos.insert(*player_id, (*x, *y));
                let killer = old.and_then(|(ox, oy)| {
                    fins.iter()
                        .rev()
                        .take_while(|(_, fts, _)| e.ts - fts <= 300)
                        .find(|(_, _, o)| {
                            let end = o.points.last().unwrap();
                            f64::from(end.x - ox).hypot(f64::from(end.y - oy)) <= 4.5
                        })
                        .map(|(_, _, o)| {
                            let begun = begins
                                .iter()
                                .rev()
                                .find(|(p, a, v, _)| {
                                    *p == o.player_id && *a == o.angle_degrees && *v == o.velocity
                                })
                                .map_or(i64::MIN, |b| b.3);
                            (o.player_id, begun)
                        })
                });
                repos.entry(*player_id).or_default().push((e.ts, killer))
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => begins.push((*player_id, *angle, *velocity, e.ts)),
            ev @ ServerEvent::ShotFin { .. } => {
                if let Ok(o) = ShotObservation::try_from(ev)
                    && !fins.iter().any(|(_, _, f)| *f == o)
                {
                    fins.push((e.seq, e.ts, o));
                }
            }
            _ => {}
        }
    }
    // Each bot's state (shared world events and its own) at every decision.
    let mut states: HashMap<&str, (BotState, usize)> = HashMap::new();
    let mut at_decision: Vec<(BotState, usize)> = Vec::with_capacity(decisions.len());
    for d in decisions {
        let (st, cur) = states.entry(&d.bot).or_default();
        while *cur < events.len() && events[*cur].seq < d.seq {
            let e = &events[*cur];
            if e.bot.as_ref().is_none_or(|b| *b == d.bot) {
                st.observe_at(&e.event, e.ts);
            }
            *cur += 1;
        }
        at_decision.push((st.clone(), *cur));
    }
    let paths_before = |seq: i64| -> Vec<ShotObservation> {
        fins.iter()
            .filter(|(s, _, _)| *s < seq)
            .map(|(_, _, o)| o.clone())
            .collect()
    };
    let advance = |state: &BotState, bot: &str, mut cur: usize, until_ts: i64| -> BotState {
        let mut st = state.clone();
        let own = st.own_id();
        while cur < events.len() && events[cur].ts <= until_ts {
            let e = &events[cur];
            cur += 1;
            if e.bot.as_ref().is_some_and(|b| b != bot) {
                continue;
            }
            // Our recorded shots did not happen in this replay.
            if let ServerEvent::ShotBegin { player_id, .. } = &e.event
                && Some(*player_id) == own
            {
                continue;
            }
            st.observe_at(&e.event, e.ts);
        }
        st
    };
    // Beliefs with their residual corrections, built once per decision.
    let built: Vec<Option<MapBelief>> = par_map(decisions.len(), threads, |i| {
        beliefs[i]
            .as_ref()
            .map(|b| b.belief(&paths_before(decisions[i].seq)))
    });
    let run = |i: usize| -> Option<Outcome> {
        let Decision {
            ts: dts, bot, data, ..
        } = &decisions[i];
        let (st0, cur) = &at_decision[i];
        let own = st0.own_id()?;
        built[i].as_ref()?;
        let e0 = data["energy"].as_f64().unwrap_or(0.0);
        let mut o = Outcome::default();
        let mut k = 0;
        loop {
            let e = e0 + ENERGY_RATE * k as f64;
            let step_ts = dts + k * 1000;
            // The map ended while waiting: no shot in this map.
            if step_ts >= world.end_ts {
                break;
            }
            let mut st = if k == 0 {
                st0.clone()
            } else {
                advance(st0, bot, *cur, step_ts)
            };
            // The latest decision's belief at this time (the recorded game moved on).
            let latest = decisions.partition_point(|d| d.ts <= step_ts);
            let belief = (i..latest).rev().find_map(|j| built[j].as_ref())?;
            st.set_map_belief(Some(belief.clone()));
            st.set_clock_ms(step_ts);
            let mut policy_settings = settings;
            policy_settings.angle_step_degrees =
                data["angle_step_degrees"].as_u64().unwrap_or(10) as u16;
            let policy = TargetingPolicy::new(policy_settings);
            let started = Instant::now();
            let choice = policy.choose(&st, e);
            o.choose_ms += started.elapsed().as_secs_f64() * 1e3;
            if let Some(c) = choice {
                let Some((path, players)) = fly(&truth, &st, own, c.velocity, c.angle_degrees)
                else {
                    break;
                };
                let fire = step_ts as f64 + FIRE_LATENCY_MS;
                let impact = fire + (path.points.len() - 1) as f64 * 1000.0 / st.step_rate();
                o.fired = true;
                o.predicted_hit = c.predicted_hit;
                o.velocity = f64::from(c.velocity);
                o.wait_s = k as f64;
                if let Termination::PlayerCollision(p) = path.termination
                    && p != own
                {
                    o.hit = true;
                    o.wasted = repos.get(&p).is_some_and(|r| {
                        r.iter().any(|(t, killer)| {
                            (*t as f64) > step_ts as f64
                                && (*t as f64) < impact
                                && !killer
                                    .is_some_and(|(shooter, begun)| shooter == own && begun >= *dts)
                        })
                    });
                }
                o.miss_distance =
                    miss_distance(&path.points, &players, own, c.predicted_victim, o.hit);
                break;
            }
            if e >= ENERGY_FULL {
                break;
            }
            k += 1;
        }
        Some(o)
    };
    par_map(decisions.len(), threads, run)
        .into_iter()
        .flatten()
        .collect()
}

/// `f(0..n)` on `threads` scoped threads, in order.
pub fn par_map<T: Send>(n: usize, threads: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let threads = threads.clamp(1, n.max(1));
    let mut out: Vec<Option<T>> = (0..n).map(|_| None).collect();
    let chunk = n.div_ceil(threads).max(1);
    std::thread::scope(|s| {
        for (t, slot) in out.chunks_mut(chunk).enumerate() {
            let f = &f;
            s.spawn(move || {
                for (k, v) in slot.iter_mut().enumerate() {
                    *v = Some(f(t * chunk + k));
                }
            });
        }
    });
    out.into_iter().map(|v| v.unwrap()).collect()
}
