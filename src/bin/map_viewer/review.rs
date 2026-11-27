//! Post-mortems: compare what a recorded belief predicted with what was observed.
//!
//! Observed paths come from the event stream; predictions are re-simulated from the recorded belief
//! with the library's physics. Outcomes read off an observed path are inferences from endpoints, not
//! reasons reported by the server.
use super::*;
use einsteinbot::simulation::PredictedTrajectory;
use einsteinbot::strategy::MapBelief;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// A path departs from the observed one when it is farther than this many world units.
const DIVERGENCE_DISTANCE: f64 = 10.0;
/// An endpoint this close to a surface or player counts as touching it.
const CONTACT_TOLERANCE: f64 = 2.0;
/// Observed shots scored per decision for the held-out prediction error.
const HORIZON_SHOTS: usize = 5;
const MAX_SAMPLES: usize = 2000;
/// An endpoint within this distance of the margin that ends shots counts as leaving the field.
const MARGIN_SLACK: f64 = 30.0;

/// A completed shot, with where every player stood when it began.
struct ObservedShot {
    begin_sequence: i64,
    end_sequence: i64,
    player_id: u32,
    angle: f64,
    velocity: f64,
    points: Vec<Point>,
    players: Vec<(u32, Vec2)>,
}

impl ObservedShot {
    fn observation(&self) -> ShotObservation {
        ShotObservation {
            player_id: self.player_id,
            angle_degrees: self.angle,
            velocity: self.velocity,
            points: self.points.clone(),
        }
    }
}

/// Pairs every `ShotBegin` with its `ShotFin` (same player, angle and velocity; the begin whose
/// player was launched nearest the first observed point wins).
fn observed_shots(events: Vec<(i64, Value)>) -> Vec<ObservedShot> {
    struct Pending {
        sequence: i64,
        player_id: u32,
        angle: f64,
        velocity: f64,
        launch: Option<Point>,
        players: Vec<(u32, Vec2)>,
    }
    let mut positions = BTreeMap::<u32, Point>::new();
    let mut pending = Vec::<Pending>::new();
    let mut shots = Vec::new();
    for (sequence, stored) in events {
        let Ok(RecordedEvent::Inbound(event)) = serde_json::from_value(stored) else {
            continue;
        };
        match event {
            ServerEvent::PlayerPosition { player_id, x, y } => {
                positions.insert(player_id, Point { x, y });
            }
            ServerEvent::PlayerLeave { player_id } => {
                positions.remove(&player_id);
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => pending.push(Pending {
                sequence,
                player_id,
                angle,
                velocity,
                launch: positions.get(&player_id).cloned(),
                players: positions
                    .iter()
                    .map(|(&id, p)| (id, Vec2::new(f64::from(p.x), f64::from(p.y))))
                    .collect(),
            }),
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                points,
            } => {
                let Some(first) = points.first() else {
                    continue;
                };
                let distance = |launch: &Option<Point>| {
                    launch.as_ref().map_or(f64::INFINITY, |launch| {
                        f64::from(launch.x - first.x).hypot(f64::from(launch.y - first.y))
                    })
                };
                let Some(index) = pending
                    .iter()
                    .enumerate()
                    .filter(|(_, p)| {
                        p.player_id == player_id && p.angle == angle && p.velocity == velocity
                    })
                    .min_by(|(_, a), (_, b)| distance(&a.launch).total_cmp(&distance(&b.launch)))
                    .map(|(index, _)| index)
                else {
                    continue;
                };
                let begun = pending.remove(index);
                shots.push(ObservedShot {
                    begin_sequence: begun.sequence,
                    end_sequence: sequence,
                    player_id,
                    angle,
                    velocity,
                    points,
                    players: begun.players,
                });
            }
            _ => {}
        }
    }
    shots.sort_by_key(|shot| shot.begin_sequence);
    shots
}

/// Simulates `shot` on `map`, the fitted planets of a recorded belief.
fn predict(
    map: &PlanetMap,
    shot: &ShotObservation,
    players: &[(u32, Vec2)],
    samples: usize,
) -> Result<PredictedTrajectory, String> {
    let samples = samples.clamp(3, MAX_SAMPLES);
    simulate_targeting_shot(
        map,
        shot,
        WorldParameters::default(),
        samples,
        players,
        PLAYER_RADIUS,
    )
}

struct PathScore {
    rmse: f64,
    compared: usize,
    diverges_at: Option<usize>,
}

/// Per-coordinate RMSE over the timed samples (the project's usual path error), and where the
/// paths first part by more than `DIVERGENCE_DISTANCE`. A collision appends its last point
/// mid-tick on either side, so that point is not a timed sample. A path that stops earlier than
/// the other is held at its last timed point, so a prediction that ends early is penalized.
fn score_path(observed: &[Point], predicted: &PredictedTrajectory) -> Option<PathScore> {
    let timed_predicted = predicted.points.len().saturating_sub(usize::from(!matches!(
        predicted.termination,
        Termination::SampleLimit
    )));
    let timed_observed = observed.len().saturating_sub(1);
    // One timed sample is the shared launch point; it says nothing about the model.
    if timed_observed < 2 || timed_predicted == 0 {
        return None;
    }
    let count = timed_observed.max(timed_predicted);
    let errors = (0..count)
        .map(|i| {
            let o = &observed[i.min(timed_observed - 1)];
            let p = &predicted.points[i.min(timed_predicted - 1)];
            f64::from(o.x - p.x).hypot(f64::from(o.y - p.y))
        })
        .collect::<Vec<_>>();
    Some(PathScore {
        rmse: (errors.iter().map(|e| e * e).sum::<f64>() / (2 * errors.len()) as f64).sqrt(),
        compared: count,
        diverges_at: errors.iter().position(|e| *e > DIVERGENCE_DISTANCE),
    })
}

fn gap_to_surface(point: &Point, planets: &[Planet]) -> Option<Value> {
    planets
        .iter()
        .enumerate()
        .map(|(index, planet)| {
            let gap = f64::from(point.x - planet.x as f32)
                .hypot(f64::from(point.y - planet.y as f32))
                - planet.radius;
            (index, gap)
        })
        .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()))
        .map(|(index, gap)| json!({"index": index, "gap": gap}))
}

/// What the end of an observed path looks like. An inference from geometry only.
fn infer_outcome(
    shot: &ObservedShot,
    belief_planets: &[Planet],
    actual: Option<&[Planet]>,
) -> Value {
    let Some(end) = shot.points.last() else {
        return json!({"kind": "unknown"});
    };
    let world = WorldParameters::default();
    if let Some((id, distance)) = shot
        .players
        .iter()
        .filter(|(id, _)| *id != shot.player_id)
        .map(|(id, p)| (*id, (f64::from(end.x) - p.x).hypot(f64::from(end.y) - p.y)))
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .filter(|(_, distance)| *distance <= PLAYER_RADIUS + CONTACT_TOLERANCE)
    {
        return json!({"kind": "player", "player_id": id, "distance": distance});
    }
    let (planets, source) = actual.map_or((belief_planets, "estimated"), |a| (a, "disclosed"));
    if let Some(surface) = gap_to_surface(end, planets).filter(|surface| {
        surface["gap"]
            .as_f64()
            .is_some_and(|gap| gap.abs() <= CONTACT_TOLERANCE)
    }) {
        return json!({"kind": "planet", "source": source, "index": surface["index"], "gap": surface["gap"]});
    }
    let (x, y) = (f64::from(end.x), f64::from(end.y));
    if x <= -world.margin + MARGIN_SLACK
        || y <= -world.margin + MARGIN_SLACK
        || x >= world.battlefield_width + world.margin - MARGIN_SLACK
        || y >= world.battlefield_height + world.margin - MARGIN_SLACK
    {
        return json!({"kind": "battlefield_margin"});
    }
    if shot.points.len() >= MAX_SAMPLES {
        return json!({"kind": "sample_limit"});
    }
    json!({"kind": "unexplained"})
}

/// The inferred outcome of every completed shot in `events`, keyed by its `ShotFin` sequence:
/// `player` (hit a player), `planet` (ended on a planet surface: the truth map when disclosed,
/// else the belief's planets), `left_field`, or `unknown` (anything else, including a sample
/// limit). Shots without a matching `ShotBegin` are absent.
pub fn path_outcomes(
    events: &[(i64, Value)],
    belief_planets: &[Planet],
    actual: Option<&[Planet]>,
) -> HashMap<i64, &'static str> {
    let shot_events = events
        .iter()
        .filter(|(_, record)| {
            matches!(
                record["event"]["type"].as_str(),
                Some("shot_begin" | "shot_fin" | "player_position" | "player_leave")
            )
        })
        .cloned()
        .collect();
    observed_shots(shot_events)
        .iter()
        .map(|shot| {
            let outcome = match infer_outcome(shot, belief_planets, actual)["kind"].as_str() {
                Some("player") => "player",
                Some("planet") => "planet",
                Some("battlefield_margin") => "left_field",
                _ => "unknown",
            };
            (shot.end_sequence, outcome)
        })
        .collect()
}

/// `GET /api/shot-review?id=&seq=[&repairs=1]` for a decision `seq`.
pub fn shot_review(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let want_repairs = parameters.get("repairs") == Some(&"1");
    let connection = open_database(path)?;
    let data = read_decision(&connection, capture_id, sequence)?;
    let belief = decode_belief(&data["map_belief"])?;
    let bot_state = BotState::from_diagnostic_snapshot(&data["bot_state"])
        .map_err(|error| error.to_string())?;
    let own_id = bot_state
        .own_id()
        .ok_or("captured decision has no own player id")?;
    let choice = &data["choice"];
    let decision = json!({
        "sequence": sequence,
        "own_id": own_id,
        "velocity": choice["velocity"],
        "angle_degrees": choice["angle_degrees"],
        "reason": choice["reason"],
        "predicted_hit": choice["predicted_hit"],
        "predicted_victim": choice["predicted_victim"],
        "miss_distance": choice["miss_distance"],
    });
    let (Some(velocity), Some(angle)) = (
        choice["velocity"].as_f64(),
        choice["angle_degrees"].as_f64(),
    ) else {
        return Ok(
            json!({"decision": decision, "shot": null, "reason": "this decision chose no shot", "variants": []}),
        );
    };
    let events = read_events_before(&connection, capture_id, i64::MAX)?;
    let actual_maps = disclosed_maps(&connection, capture_id)?;
    let actual = actual_maps
        .iter()
        .rev()
        .find(|(planets_sequence, _)| *planets_sequence < sequence)
        .map(|(_, planets)| planets.clone());
    let shots = observed_shots(events);
    // The decision's shot began before this bot's next decision and within the same map epoch.
    let next_decision: Option<i64> = connection
        .query_row(
            "SELECT MIN(sequence) FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' AND bot_name=(SELECT bot_name FROM diagnostics WHERE capture_id=?1 AND sequence=?2) AND sequence>?2",
            params![capture_id, sequence],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let epoch_end = actual_maps
        .iter()
        .map(|(planets_sequence, _)| *planets_sequence)
        .find(|planets_sequence| *planets_sequence > sequence);
    let window_end = [next_decision, epoch_end]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(i64::MAX);
    let matched = shots.iter().find(|shot| {
        shot.begin_sequence > sequence
            && shot.begin_sequence < window_end
            && shot.player_id == own_id
            && (shot.velocity - velocity).abs() < 1e-6
            && (shot.angle - angle).abs() < 1e-6
    });
    let Some(shot) = matched else {
        return Ok(json!({
            "decision": decision,
            "shot": null,
            "reason": "no finished shot by this player with the same velocity and angle follows this decision in the capture",
            "variants": [],
        }));
    };
    let decision_launch = bot_state.player_positions().get(&own_id).cloned();
    let launch_offset = decision_launch
        .as_ref()
        .zip(shot.points.first())
        .map(|(a, b)| f64::from(a.x - b.x).hypot(f64::from(a.y - b.y)));
    let belief_planets = belief
        .as_ref()
        .map(|b| b.map.planets.clone())
        .unwrap_or_default();
    let observation = shot.observation();
    let samples = shot.points.len() + 2;
    let mut variants = Vec::new();
    let mut divergence = Value::Null;
    if let Some(belief) = &belief {
        let predicted = predict(&belief.map, &observation, &shot.players, samples)?;
        variants.push(variant_json(
            "physical",
            "Fitted planets only",
            true,
            Some(&predicted),
            &shot.points,
        ));
        divergence = divergence_json(&shot.points, &predicted, &belief_planets, actual.as_deref());
        if let Some(planets) = &actual {
            let map = PlanetMap {
                planets: planets.clone(),
            };
            let predicted = simulate_targeting_shot(
                &map,
                &observation,
                WorldParameters::default(),
                samples.clamp(3, MAX_SAMPLES),
                &shot.players,
                PLAYER_RADIUS,
            )?;
            variants.push(variant_json(
                "actual",
                "Disclosed actual map",
                false,
                Some(&predicted),
                &shot.points,
            ));
        }
    }
    let repairs = if want_repairs {
        match (&belief, &actual) {
            (Some(belief), Some(actual)) => {
                repair_experiments(belief, actual, &observation, &shot.players, samples)?
            }
            _ => json!({"error": "repairs need a recorded belief and a disclosed map"}),
        }
    } else {
        Value::Null
    };
    Ok(json!({
        "decision": decision,
        "shot": {
            "begin_sequence": shot.begin_sequence,
            "end_sequence": shot.end_sequence,
            "points": json!(&shot.points),
            "launch_offset": launch_offset,
            "outcome": infer_outcome(shot, &belief_planets, actual.as_deref()),
        },
        "variants": variants,
        "divergence": divergence,
        "repairs": repairs,
    }))
}

fn variant_json(
    key: &str,
    label: &str,
    as_used: bool,
    predicted: Option<&PredictedTrajectory>,
    observed: &[Point],
) -> Value {
    let Some(predicted) = predicted else {
        return json!({"key": key, "label": label, "as_used": as_used, "available": false});
    };
    let score = score_path(observed, predicted);
    let end_error = predicted
        .points
        .last()
        .zip(observed.last())
        .map(|(p, o)| f64::from(p.x - o.x).hypot(f64::from(p.y - o.y)));
    json!({
        "key": key,
        "label": label,
        "as_used": as_used,
        "available": true,
        "points": json!(&predicted.points),
        "termination": termination_name(predicted.termination),
        "rmse": score.as_ref().map(|s| s.rmse),
        "compared_samples": score.as_ref().map(|s| s.compared),
        "diverges_at": score.as_ref().and_then(|s| s.diverges_at),
        "end_error": end_error,
    })
}

/// Where the as-used prediction first leaves the observed path, and which planets are nearest there.
fn divergence_json(
    observed: &[Point],
    predicted: &PredictedTrajectory,
    belief_planets: &[Planet],
    actual: Option<&[Planet]>,
) -> Value {
    let Some(index) = score_path(observed, predicted).and_then(|score| score.diverges_at) else {
        return Value::Null;
    };
    // The index can pass the observed end when the prediction flew on after the shot ended.
    let point = observed
        .get(index)
        .or(observed.last())
        .expect("scored paths are not empty");
    json!({
        "index": index,
        "point": {"x": point.x, "y": point.y},
        "nearest_estimated": gap_to_surface(point, belief_planets),
        "nearest_disclosed": actual.and_then(|planets| gap_to_surface(point, planets)),
    })
}

/// Model sensitivity: how much correcting one planet of the belief moves the as-used prediction
/// toward the observed path. Needs the disclosed map; says nothing about the actual cause.
fn repair_experiments(
    belief: &MapBelief,
    actual: &[Planet],
    observation: &ShotObservation,
    players: &[(u32, Vec2)],
    samples: usize,
) -> Result<Value, String> {
    let rmse_with = |map: &PlanetMap| -> Result<Option<f64>, String> {
        Ok(score_path(
            &observation.points,
            &predict(map, observation, players, samples)?,
        )
        .map(|score| score.rmse))
    };
    let baseline = rmse_with(&belief.map)?;
    let pairs = match_planets(&belief.map.planets, actual);
    let mut rows = Vec::new();
    let mut all = belief.map.clone();
    for (i, truth) in actual.iter().enumerate() {
        let pair = pairs.iter().find(|pair| pair.1 == i);
        let (kind, map) = match pair {
            None => {
                let mut map = belief.map.clone();
                map.planets.push(truth.clone());
                all.planets.push(truth.clone());
                ("add_missing", map)
            }
            Some(&(k, _, distance)) => {
                let estimate = &belief.map.planets[k];
                if distance <= 10.0 && (estimate.radius - truth.radius).abs() <= 5.0 {
                    continue;
                }
                let mut map = belief.map.clone();
                map.planets[k] = truth.clone();
                all.planets[k] = truth.clone();
                ("correct_estimate", map)
            }
        };
        let rmse = rmse_with(&map)?;
        rows.push(json!({
            "disclosed_index": i,
            "kind": kind,
            "estimated_index": pair.map(|pair| pair.0),
            "rmse": rmse,
            "improvement": baseline.zip(rmse).map(|(before, after)| before - after),
        }));
    }
    rows.sort_by(|a, b| {
        b["improvement"]
            .as_f64()
            .unwrap_or(f64::NEG_INFINITY)
            .total_cmp(&a["improvement"].as_f64().unwrap_or(f64::NEG_INFINITY))
    });
    Ok(json!({
        "baseline_rmse": baseline,
        "all_repaired_rmse": rmse_with(&all)?,
        "rows": rows,
        "note": "Model sensitivity, not proof of cause: each row changes one planet of the recorded belief.",
    }))
}

/// One entry per capture, replaced when the capture's newest sequence moves (live recording).
type PredictiveCache = Mutex<HashMap<(PathBuf, i64), (i64, Value)>>;

/// `GET /api/predictive?id=`: per decision, the path error of its belief against the next shots
/// that were actually observed (no disclosed map needed).
pub fn predictive_error(
    path: &PathBuf,
    parameters: &BTreeMap<&str, &str>,
) -> Result<Value, String> {
    static CACHE: OnceLock<PredictiveCache> = OnceLock::new();
    let capture_id = required_id(parameters, "id")?;
    let connection = open_database(path)?;
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
    let events = read_events_before(&connection, capture_id, i64::MAX)?;
    let planets_sequences = disclosed_maps(&connection, capture_id)?
        .into_iter()
        .map(|(sequence, _)| sequence)
        .collect::<Vec<_>>();
    let shots = observed_shots(events);
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
        let data: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        let Some(belief) = decode_belief(&data["map_belief"])? else {
            decisions.push(json!({"sequence": sequence, "shots": 0, "median_rmse": null, "status": "no_belief"}));
            continue;
        };
        // Shots after a new disclosed map belong to another map epoch.
        let epoch_end = planets_sequences
            .iter()
            .copied()
            .find(|planets| *planets > sequence);
        let mut errors = Vec::new();
        let mut later_shots = 0;
        for shot in shots
            .iter()
            .filter(|shot| shot.begin_sequence > sequence)
            .filter(|shot| epoch_end.is_none_or(|end| shot.begin_sequence < end))
            .take(HORIZON_SHOTS)
        {
            later_shots += 1;
            let predicted = predict(
                &belief.map,
                &shot.observation(),
                &shot.players,
                shot.points.len() + 2,
            )?;
            if let Some(score) = score_path(&shot.points, &predicted) {
                errors.push(score.rmse);
            }
        }
        errors.sort_by(f64::total_cmp);
        let status = if later_shots == 0 {
            "no_later_shots"
        } else if errors.is_empty() {
            "too_short_to_score"
        } else {
            "scored"
        };
        decisions.push(json!({
            "sequence": sequence,
            "status": status,
            "shots": errors.len(),
            "median_rmse": median(&errors),
            "max_rmse": errors.last(),
        }));
    }
    let result = json!({"horizon_shots": HORIZON_SHOTS, "decisions": decisions});
    cache
        .lock()
        .map_err(|_| "cache poisoned")?
        .insert(key, (last_sequence, result.clone()));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(sequence: i64, event: Value) -> (i64, Value) {
        (sequence, json!({"direction": "inbound", "event": event}))
    }

    fn point(x: f32, y: f32) -> Point {
        Point { x, y }
    }

    #[test]
    fn shots_pair_begin_and_fin_and_remember_where_players_stood() {
        let path = json!([{"x": 200.0, "y": 400.0}, {"x": 210.0, "y": 400.0}]);
        let events = vec![
            event(
                1,
                json!({"type": "player_position", "player_id": 1, "x": 200.0, "y": 400.0}),
            ),
            event(
                2,
                json!({"type": "player_position", "player_id": 2, "x": 900.0, "y": 100.0}),
            ),
            event(
                3,
                json!({"type": "shot_begin", "player_id": 1, "angle": 0.0, "velocity": 8.0}),
            ),
            event(
                4,
                json!({"type": "shot_fin", "player_id": 1, "angle": 0.0, "velocity": 8.0, "points": path}),
            ),
            event(
                5,
                json!({"type": "shot_begin", "player_id": 2, "angle": 90.0, "velocity": 3.0}),
            ),
        ];
        let shots = observed_shots(events);
        assert_eq!(shots.len(), 1, "an unfinished shot is not an observed shot");
        let shot = &shots[0];
        assert_eq!(
            (shot.begin_sequence, shot.end_sequence, shot.player_id),
            (3, 4, 1)
        );
        assert_eq!(shot.players.len(), 2);
    }

    #[test]
    fn path_scores_report_where_the_prediction_leaves_the_observed_path() {
        let observed = vec![
            point(0.0, 0.0),
            point(1.0, 0.0),
            point(2.0, 0.0),
            point(3.0, 0.0),
        ];
        let predicted = PredictedTrajectory {
            points: vec![
                point(0.0, 0.0),
                point(1.0, 0.0),
                point(2.0, 15.0),
                point(3.0, 30.0),
            ],
            termination: Termination::PlanetCollision(0),
        };
        let score = score_path(&observed, &predicted).unwrap();
        assert_eq!(score.compared, 3);
        assert_eq!(score.diverges_at, Some(2));
        assert!((score.rmse - (225.0_f64 / 6.0).sqrt()).abs() < 1e-9);
        let identical = PredictedTrajectory {
            points: observed.clone(),
            termination: Termination::PlanetCollision(0),
        };
        let perfect = score_path(&observed, &identical).unwrap();
        assert_eq!((perfect.diverges_at, perfect.rmse), (None, 0.0));
    }

    #[test]
    fn a_prediction_that_stops_early_is_penalized_and_trivial_shots_are_not_scored() {
        let observed = (0..6)
            .map(|i| point(20.0 * i as f32, 0.0))
            .collect::<Vec<_>>();
        let stops_early = PredictedTrajectory {
            points: vec![point(0.0, 0.0), point(20.0, 0.0), point(40.0, 0.0)],
            termination: Termination::PlanetCollision(0),
        };
        // Held at its last timed sample (20, 0) while the observed shot flies on.
        let score = score_path(&observed, &stops_early).unwrap();
        assert_eq!(score.diverges_at, Some(2));
        assert_eq!(score.compared, 5);
        let launch_only = vec![point(0.0, 0.0), point(1.0, 0.0)];
        assert!(score_path(&launch_only, &stops_early).is_none());
    }

    #[test]
    fn divergence_past_the_observed_end_points_at_the_last_observed_sample() {
        let observed = (0..3).map(|i| point(i as f32, 0.0)).collect::<Vec<_>>();
        let flies_on = PredictedTrajectory {
            points: (0..12)
                .map(|i| point(if i <= 4 { i as f32 } else { 100.0 }, 0.0))
                .collect(),
            termination: Termination::SampleLimit,
        };
        let divergence = divergence_json(&observed, &flies_on, &[], None);
        assert_eq!(divergence["index"], 5);
        assert_eq!(divergence["point"]["x"], 2.0);
    }

    #[test]
    fn endpoint_outcomes_are_geometric_inferences() {
        let shot = |end: Point| ObservedShot {
            begin_sequence: 1,
            end_sequence: 2,
            player_id: 1,
            angle: 0.0,
            velocity: 5.0,
            points: vec![point(0.0, 0.0), end],
            players: vec![(1, Vec2::new(0.0, 0.0)), (2, Vec2::new(500.0, 500.0))],
        };
        let planet = Planet {
            x: 100.0,
            y: 100.0,
            radius: 30.0,
            mass: 1000.0,
        };
        assert_eq!(
            infer_outcome(&shot(point(501.0, 500.0)), &[], None)["kind"],
            "player"
        );
        let planet_outcome = infer_outcome(&shot(point(130.0, 100.0)), &[], Some(&[planet]));
        assert_eq!(
            (
                planet_outcome["kind"].clone(),
                planet_outcome["source"].clone()
            ),
            (json!("planet"), json!("disclosed"))
        );
        assert_eq!(
            infer_outcome(&shot(point(300.0, 50.0)), &[], None)["kind"],
            "unexplained"
        );
        assert_eq!(
            infer_outcome(&shot(point(-480.0, 50.0)), &[], None)["kind"],
            "battlefield_margin"
        );
    }

    #[test]
    fn path_outcomes_key_each_finished_shot_by_its_fin_sequence() {
        let to_player = json!([{"x": 200.0, "y": 400.0}, {"x": 900.0, "y": 101.0}]);
        let nowhere = json!([{"x": 900.0, "y": 100.0}, {"x": 700.0, "y": 300.0}]);
        let events = vec![
            event(
                1,
                json!({"type": "player_position", "player_id": 1, "x": 200.0, "y": 400.0}),
            ),
            event(
                2,
                json!({"type": "player_position", "player_id": 2, "x": 900.0, "y": 100.0}),
            ),
            event(
                3,
                json!({"type": "shot_begin", "player_id": 1, "angle": 0.0, "velocity": 8.0}),
            ),
            event(
                4,
                json!({"type": "shot_begin", "player_id": 2, "angle": 9.0, "velocity": 8.0}),
            ),
            event(
                5,
                json!({"type": "shot_fin", "player_id": 1, "angle": 0.0, "velocity": 8.0, "points": to_player}),
            ),
            event(
                6,
                json!({"type": "shot_fin", "player_id": 2, "angle": 9.0, "velocity": 8.0, "points": nowhere}),
            ),
        ];
        let outcomes = path_outcomes(&events, &[], None);
        assert_eq!(outcomes.get(&5), Some(&"player"));
        assert_eq!(outcomes.get(&6), Some(&"unknown"));
        assert_eq!(outcomes.len(), 2);
    }
}
