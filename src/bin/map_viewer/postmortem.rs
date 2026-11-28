//! Decision post-mortems (P26): why each recorded decision turned out as it did.
//!
//! For a decision with a shot, three futures from the same launch: the path the bot predicted
//! (its belief plus the residual correction it applied), the path on the disclosed true map with
//! the players frozen at the decision, and the observed shot. The cause comes from comparing
//! them (`classify`); for a map error, the first point where prediction and truth part
//! (divergence) and the true planet nearest it tell whether a planet was missing, off, or small
//! errors were amplified. Truth only scores. Waits use the recorded reason (P24
//! `decision.wait_reason`) or, in older captures, an inference from the energy.
use super::*;
use einsteinbot::simulation::{PredictedTrajectory, simulate_corrected_targeting_shot};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// A planet whose estimate is this far (or farther) from the true centre counts as missing.
const MISSING_OFFSET: f64 = 10.0;
/// ... and this far as off; below it the planet is right and small errors were amplified.
const OFF_OFFSET: f64 = 2.0;
/// Paths and truth part where they are farther apart than this (world units).
const PART_DISTANCE: f64 = 10.0;
/// Under this many fitted paths the map is too young to blame (`early_blind`).
const EARLY_PATHS: usize = 8;
/// An observed endpoint this close to a player's centre counts as hitting it (player radius 4
/// plus slack).
const HIT_DISTANCE: f64 = PLAYER_RADIUS + 2.0;
const SIMULATED_SAMPLES: usize = 600;

/// What happened after a decision, read off the events.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Aftermath {
    /// Our `ShotBegin` with the chosen velocity and angle, and its `ShotFin`.
    pub begin: Option<i64>,
    pub fin: Option<(i64, Vec<Point>)>,
    /// Our own position was reported again before the shot began (launch moved) or while it flew
    /// (we were killed and respawned: the server removes a dead player's shots).
    pub own_moved_before_begin: bool,
    pub own_moved_after_begin: bool,
    /// Players reported at a new position between the decision and our shot's end (respawned).
    pub moved: Vec<u32>,
}

/// The cause classes of the timeline glyphs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Hit,
    HitOther,
    MapError,
    EarlyBlind,
    TargetGone,
    WeDied,
    NotFired,
    NoFin,
    LaunchMoved,
    TimingMismatch,
    /// A predicted hit that missed on a map without disclosed truth: the cause is unknown.
    MissNoTruth,
    /// Approach or information shot (no predicted hit).
    NoPredictedHit,
    Wait,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Hit => "hit",
            Class::HitOther => "hit_other",
            Class::MapError => "map_error",
            Class::EarlyBlind => "early_blind",
            Class::TargetGone => "target_gone",
            Class::WeDied => "we_died",
            Class::NotFired => "not_fired",
            Class::NoFin => "no_fin",
            Class::LaunchMoved => "launch_moved",
            Class::TimingMismatch => "timing_mismatch",
            Class::MissNoTruth => "miss_no_truth",
            Class::NoPredictedHit => "no_predicted_hit",
            Class::Wait => "wait",
        }
    }
}

/// The cause of a decision with a shot. `victim` is the predicted victim of a hit decision (None
/// for approach and information shots), `observed_victim` the player the observed shot ended on,
/// `truth_hits_victim` whether the shot flown on the true map with the players frozen would hit
/// the predicted victim (`None`: no truth map), `paths_fitted` the belief's paths.
pub fn classify(
    victim: Option<u32>,
    after: &Aftermath,
    observed_victim: Option<u32>,
    truth_hits_victim: Option<bool>,
    paths_fitted: usize,
) -> Class {
    if after.fin.is_none() {
        return if after.begin.is_none() {
            Class::NotFired
        } else if after.own_moved_after_begin {
            Class::WeDied
        } else {
            Class::NoFin
        };
    }
    let Some(victim) = victim else {
        return Class::NoPredictedHit;
    };
    if observed_victim == Some(victim) {
        return Class::Hit;
    }
    if observed_victim.is_some() {
        return Class::HitOther;
    }
    let victim_moved = after.moved.contains(&victim);
    match truth_hits_victim {
        Some(true) if victim_moved => Class::TargetGone,
        Some(true) if after.own_moved_before_begin => Class::LaunchMoved,
        Some(true) => Class::TimingMismatch,
        Some(false) if paths_fitted < EARLY_PATHS => Class::EarlyBlind,
        Some(false) => Class::MapError,
        None if victim_moved => Class::TargetGone,
        None => Class::MissNoTruth,
    }
}

/// What a map error was, from the true planet nearest the point where prediction and truth part:
/// `offset` is the distance from that planet to the nearest estimated centre.
pub fn map_error_kind(offset: f64) -> &'static str {
    if offset >= MISSING_OFFSET {
        "missing_planet"
    } else if offset > OFF_OFFSET {
        "planet_off"
    } else {
        "small_errors_amplified"
    }
}

/// Our shot after the decision at `sequence` (velocity and angle as chosen), up to the next
/// `planets` event.
pub fn aftermath(
    events: &[(i64, Value)],
    sequence: i64,
    own: u32,
    velocity: f64,
    angle: f64,
) -> Aftermath {
    let mut out = Aftermath::default();
    let same = |e: &Value| {
        e["player_id"].as_u64() == Some(u64::from(own))
            && e["velocity"].as_f64() == Some(velocity)
            && e["angle"]
                .as_f64()
                .is_some_and(|a| (a - angle).abs() < 0.51)
    };
    for (s, record) in events.iter().filter(|(s, _)| *s > sequence) {
        let e = &record["event"];
        match e["type"].as_str() {
            Some("planets") => break,
            Some("shot_begin") if out.begin.is_none() && same(e) => out.begin = Some(*s),
            Some("shot_fin") if out.begin.is_some() && same(e) => {
                let points = serde_json::from_value(e["points"].clone()).unwrap_or_default();
                out.fin = Some((*s, points));
                break;
            }
            Some("player_position") => {
                let Some(id) = e["player_id"].as_u64().map(|id| id as u32) else {
                    continue;
                };
                if id == own {
                    if out.begin.is_none() {
                        out.own_moved_before_begin = true;
                    } else {
                        out.own_moved_after_begin = true;
                    }
                } else if !out.moved.contains(&id) {
                    out.moved.push(id);
                }
            }
            _ => {}
        }
        // A shot not begun within a few hundred events was not fired for this decision.
        if out.begin.is_none() && *s > sequence + 400 {
            break;
        }
    }
    out
}

/// The wait reason: recorded (P24) or inferred from the energy (older captures).
fn wait_reason(data: &Value) -> (String, &'static str) {
    if let Some(reason) = data["decision"]["wait_reason"].as_str() {
        return (reason.to_owned(), "recorded");
    }
    let energy = data["energy"].as_f64().unwrap_or(f64::NAN);
    let reason = if energy < 2.0 {
        "low_energy"
    } else if energy < 15.0 {
        "no_hit_low_energy"
    } else {
        "no_target_or_all_doomed"
    };
    (reason.to_owned(), "inferred")
}

fn points_json(points: &[Point]) -> Vec<[f32; 2]> {
    points.iter().map(|p| [p.x, p.y]).collect()
}

fn part_at(a: &[Point], b: &[Point]) -> Option<usize> {
    a.iter()
        .zip(b)
        .position(|(p, q)| f64::from(p.x - q.x).hypot(f64::from(p.y - q.y)) > PART_DISTANCE)
}

/// Everything known about one decision; `detail` adds the paths and planets for the card.
fn analyse(
    events: &[(i64, Value)],
    feed: &[(i64, Option<bool>)],
    disclosed: &[(i64, Vec<Planet>)],
    sequence: i64,
    data: &Value,
    detail: bool,
) -> Result<Value, String> {
    let choice = &data["choice"];
    let alternatives = &data["decision"]["top_hits"];
    if choice.is_null() || choice["velocity"].is_null() {
        let (reason, source) = wait_reason(data);
        return Ok(json!({
            "sequence": sequence,
            "class": Class::Wait.name(),
            "wait_reason": reason,
            "wait_reason_source": source,
            "alternatives": alternatives,
        }));
    }
    let state =
        BotState::from_diagnostic_snapshot(&data["bot_state"]).map_err(|e| e.to_string())?;
    let own = state.own_id().ok_or("decision has no own player id")?;
    let start = state
        .player_positions()
        .get(&own)
        .cloned()
        .ok_or("decision has no own position")?;
    let players: Vec<(u32, Vec2)> = state
        .player_positions()
        .iter()
        .map(|(&id, p)| (id, Vec2::new(f64::from(p.x), f64::from(p.y))))
        .collect();
    let velocity = choice["velocity"].as_f64().ok_or("no velocity")?;
    let angle = choice["angle_degrees"].as_f64().ok_or("no angle")?;
    let hit = choice["reason"].as_str() == Some("Hit");
    let victim = hit
        .then(|| choice["predicted_victim"].as_u64().map(|v| v as u32))
        .flatten();
    let after = aftermath(events, sequence, own, velocity, angle);
    let observed_victim = after.fin.as_ref().and_then(|(_, points)| {
        let end = points.last()?;
        players
            .iter()
            .filter(|(id, _)| *id != own)
            .map(|(id, p)| (*id, (f64::from(end.x) - p.x).hypot(f64::from(end.y) - p.y)))
            .filter(|(_, d)| *d <= HIT_DISTANCE)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(id, _)| id)
    });
    let truth = disclosed
        .iter()
        .rev()
        .find(|(s, _)| *s < sequence)
        .map(|(_, planets)| planets.clone());
    let shot = ShotObservation {
        player_id: own,
        angle_degrees: angle,
        velocity,
        points: vec![start],
    };
    let world = WorldParameters::default();
    let truth_path = truth
        .as_ref()
        .map(|planets| {
            simulate_targeting_shot(
                &PlanetMap {
                    planets: planets.clone(),
                },
                &shot,
                world,
                SIMULATED_SAMPLES,
                &players,
                PLAYER_RADIUS,
            )
        })
        .transpose()?;
    let truth_hits_victim = truth_path
        .as_ref()
        .map(|t| victim.is_some_and(|v| t.termination == Termination::PlayerCollision(v)));
    let paths = super::fog::epoch_paths(events, sequence);
    let lag = map_lag(data, feed, sequence);
    let (fitted, _) = super::fog::fitted_count(&paths, sequence, data, &lag);
    let class = classify(victim, &after, observed_victim, truth_hits_victim, fitted);
    let mut out = json!({
        "sequence": sequence,
        "class": class.name(),
        "reason": choice["reason"],
        "victim": victim,
        "observed_victim": observed_victim,
        "fin_sequence": after.fin.as_ref().map(|(s, _)| *s),
        "paths_fitted": fitted,
        "flight_ticks": choice["flight_ticks"],
        "victim_doom": choice["victim_doom"],
        "alternatives": alternatives,
    });
    let needs_prediction = detail || matches!(class, Class::MapError | Class::EarlyBlind);
    if !needs_prediction {
        return Ok(out);
    }
    let planets: Vec<Planet> =
        serde_json::from_value(data["map_belief"]["planets"].clone()).unwrap_or_default();
    let correction = super::review::used_correction(events, feed, sequence, data);
    let predicted: PredictedTrajectory = simulate_corrected_targeting_shot(
        &PlanetMap {
            planets: planets.clone(),
        },
        correction.as_ref(),
        &shot,
        world,
        SIMULATED_SAMPLES,
        &players,
        PLAYER_RADIUS,
    )?;
    // Where the prediction parts from truth (or, without truth, from the observed shot).
    let reference: Option<&[Point]> = truth_path
        .as_ref()
        .map(|t| t.points.as_slice())
        .or(after.fin.as_ref().map(|(_, p)| p.as_slice()));
    let part = reference.and_then(|r| {
        part_at(&predicted.points, r).or_else(|| {
            (predicted.points.len() != r.len())
                .then(|| predicted.points.len().min(r.len()).saturating_sub(1))
        })
    });
    let at = part.and_then(|i| predicted.points.get(i).cloned());
    let mut cause = Value::Null;
    if let (Some(at), Some(truth)) = (&at, &truth) {
        let (x, y) = (f64::from(at.x), f64::from(at.y));
        if let Some((index, gap)) = truth
            .iter()
            .enumerate()
            .map(|(i, q)| (i, (q.x - x).hypot(q.y - y) - q.radius))
            .min_by(|a, b| a.1.total_cmp(&b.1))
        {
            let q = &truth[index];
            let offset = planets
                .iter()
                .map(|p| (p.x - q.x).hypot(p.y - q.y))
                .fold(f64::INFINITY, f64::min);
            if matches!(class, Class::MapError | Class::EarlyBlind) {
                cause = json!({
                    "kind": map_error_kind(offset),
                    "truth_planet": index,
                    "gap": gap,
                    "estimate_offset": offset.is_finite().then_some(offset),
                });
            }
        }
    }
    out["cause"] = cause;
    out["part_index"] = json!(part);
    out["part_at"] = json!(at.as_ref().map(|p| [p.x, p.y]));
    if detail {
        let fog_risk = super::fog::decision_fog(events, sequence, data, &lag)
            .zip(at.as_ref())
            .map(|((fog, _), p)| fog.risk_at(Vec2::new(f64::from(p.x), f64::from(p.y))));
        out["fog_risk_at_part"] = json!(fog_risk);
        out["predicted"] = json!({
            "points": points_json(&predicted.points),
            "termination": termination_name(predicted.termination),
            "with_correction": correction.is_some(),
        });
        out["truth"] = json!(truth_path.as_ref().map(|t| json!({
            "points": points_json(&t.points),
            "termination": termination_name(t.termination),
        })));
        out["observed"] = json!(after.fin.as_ref().map(|(_, p)| points_json(p)));
        out["players"] = json!(
            players
                .iter()
                .map(|(id, p)| json!({"id": id, "x": p.x, "y": p.y}))
                .collect::<Vec<_>>()
        );
        out["own_id"] = json!(own);
    }
    Ok(out)
}

/// `GET /api/postmortem?id=[&seq=]`: the cause of every decision of a capture (cached), or one
/// decision with its predicted, true-map and observed paths.
pub fn postmortem_request(
    path: &PathBuf,
    parameters: &BTreeMap<&str, &str>,
) -> Result<Value, String> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, i64), (i64, Value)>>> = OnceLock::new();
    let capture_id = required_id(parameters, "id")?;
    let connection = open_database(path)?;
    let events = read_events_before(&connection, capture_id, i64::MAX)?;
    let feed = feed_events(&connection, capture_id)?;
    let disclosed = disclosed_maps(&connection, capture_id)?;
    if let Some(sequence) = parameters.get("seq") {
        let sequence = sequence
            .parse::<i64>()
            .map_err(|_| "invalid seq".to_owned())?;
        let data = read_decision(&connection, capture_id, sequence)?;
        return analyse(&events, &feed, &disclosed, sequence, &data, true);
    }
    let last_sequence: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(sequence), 0) FROM (SELECT sequence FROM events WHERE capture_id=?1 UNION ALL SELECT sequence FROM diagnostics WHERE capture_id=?1)",
            [capture_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let key = (path.clone(), capture_id);
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some((cached_sequence, cached)) = cache.lock().map_err(|_| "cache poisoned")?.get(&key)
        && *cached_sequence == last_sequence
    {
        return Ok(cached.clone());
    }
    let mut statement = connection
        .prepare("SELECT sequence, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([capture_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut decisions = Vec::new();
    for row in rows {
        let (sequence, text) = row.map_err(|error| error.to_string())?;
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        decisions.push(
            analyse(&events, &feed, &disclosed, sequence, &data, false).unwrap_or_else(
                |error| json!({"sequence": sequence, "class": "unknown", "error": error}),
            ),
        );
    }
    let result = json!({"capture_id": capture_id, "decisions": decisions});
    cache
        .lock()
        .map_err(|_| "cache poisoned")?
        .insert(key, (last_sequence, result.clone()));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fired(fin: bool) -> Aftermath {
        Aftermath {
            begin: Some(5),
            fin: fin.then(|| (9, vec![Point { x: 1.0, y: 1.0 }])),
            ..Aftermath::default()
        }
    }

    #[test]
    fn classes_follow_what_happened_to_our_shot_and_the_target() {
        assert_eq!(
            classify(Some(2), &Aftermath::default(), None, None, 30),
            Class::NotFired
        );
        let mut died = fired(false);
        died.own_moved_after_begin = true;
        assert_eq!(
            classify(Some(2), &died, None, Some(true), 30),
            Class::WeDied
        );
        assert_eq!(
            classify(Some(2), &fired(false), None, None, 30),
            Class::NoFin
        );
        assert_eq!(
            classify(Some(2), &fired(true), Some(2), None, 30),
            Class::Hit
        );
        assert_eq!(
            classify(Some(2), &fired(true), Some(3), None, 30),
            Class::HitOther
        );
        assert_eq!(
            classify(None, &fired(true), None, Some(false), 30),
            Class::NoPredictedHit
        );
        let mut gone = fired(true);
        gone.moved = vec![2];
        assert_eq!(
            classify(Some(2), &gone, None, Some(true), 30),
            Class::TargetGone
        );
        assert_eq!(classify(Some(2), &gone, None, None, 30), Class::TargetGone);
        let mut moved = fired(true);
        moved.own_moved_before_begin = true;
        assert_eq!(
            classify(Some(2), &moved, None, Some(true), 30),
            Class::LaunchMoved
        );
        assert_eq!(
            classify(Some(2), &fired(true), None, Some(true), 30),
            Class::TimingMismatch
        );
        assert_eq!(
            classify(Some(2), &fired(true), None, Some(false), 30),
            Class::MapError
        );
        assert_eq!(
            classify(Some(2), &fired(true), None, Some(false), 5),
            Class::EarlyBlind
        );
        assert_eq!(
            classify(Some(2), &fired(true), None, None, 30),
            Class::MissNoTruth
        );
    }

    #[test]
    fn map_errors_are_missing_planets_planets_off_or_amplified_small_errors() {
        assert_eq!(map_error_kind(f64::INFINITY), "missing_planet");
        assert_eq!(map_error_kind(25.0), "missing_planet");
        assert_eq!(map_error_kind(4.0), "planet_off");
        assert_eq!(map_error_kind(0.5), "small_errors_amplified");
    }

    #[test]
    fn the_aftermath_pairs_our_shot_and_sees_respawns() {
        let e = |s: i64, v: Value| (s, json!({"direction": "inbound", "event": v}));
        let events = vec![
            e(
                10,
                json!({"type": "player_position", "player_id": 2, "x": 5.0, "y": 5.0}),
            ),
            e(
                11,
                json!({"type": "shot_begin", "player_id": 1, "angle": 30.0, "velocity": 8.0}),
            ),
            e(
                12,
                json!({"type": "shot_begin", "player_id": 1, "angle": -30.0, "velocity": 8.0}),
            ),
            e(
                13,
                json!({"type": "shot_fin", "player_id": 1, "angle": 30.0, "velocity": 8.0, "points": [{"x": 1.0, "y": 2.0}]}),
            ),
            e(
                14,
                json!({"type": "player_position", "player_id": 1, "x": 5.0, "y": 5.0}),
            ),
        ];
        let after = aftermath(&events, 9, 1, 8.0, 30.0);
        assert_eq!(after.begin, Some(11));
        assert_eq!(after.fin.as_ref().map(|f| f.0), Some(13));
        assert_eq!(after.moved, vec![2]);
        assert!(!after.own_moved_before_begin && !after.own_moved_after_begin);
        // The other shot never finished and we respawned while it flew.
        let after = aftermath(&events, 9, 1, 8.0, -30.0);
        assert_eq!((after.begin, after.fin.is_none()), (Some(12), true));
        assert!(after.own_moved_after_begin);
        assert_eq!(classify(Some(2), &after, None, None, 30), Class::WeDied);
    }

    #[test]
    fn waits_use_the_recorded_reason_else_an_inference_from_energy() {
        let recorded =
            json!({"choice": null, "decision": {"wait_reason": "all_hits_doomed"}, "energy": 40.0});
        assert_eq!(
            wait_reason(&recorded),
            ("all_hits_doomed".into(), "recorded")
        );
        assert_eq!(
            wait_reason(&json!({"energy": 1.0})),
            ("low_energy".into(), "inferred")
        );
        let out = analyse(&[], &[], &[], 3, &recorded, false).unwrap();
        assert_eq!(out["class"], "wait");
        assert_eq!(out["wait_reason"], "all_hits_doomed");
    }
}
