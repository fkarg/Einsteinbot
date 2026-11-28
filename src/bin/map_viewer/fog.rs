//! Fog of war in the inspector (P26): the unseen-planet risk and pull of a recorded belief, what
//! each path taught (information in bits) and where (its footprint).
//!
//! Everything is rebuilt from the library (`estimator::unseen_fog`, `estimator::fog_timeline`)
//! from the belief's planets, its recorded `fog_model` and the paths of its map epoch that it had
//! fitted (`paths_fitted`). Captures before P26 record neither: the model is then the known count
//! of `residual_model.total_planets` or the 24 +- 4 prior, and the fitted paths are the epoch's
//! paths finished before the decision minus its recorded lag; both are marked `assumed` /
//! `inferred`.
use super::*;
use einsteinbot::estimator::{
    Fog, FogModel, PathInformation, fog_information, fog_timeline, unseen_fog,
};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Spacing of the fog grid (world units); the fog varies over tens of units.
pub const FOG_STEP: f64 = 20.0;

/// The completed paths of the map epoch that contains `sequence` (from the latest `planets` event
/// at or before it to the next one), deduplicated like the estimator's, with their `shot_fin`
/// sequences, in order.
pub fn epoch_paths(events: &[(i64, Value)], sequence: i64) -> Vec<(i64, ShotObservation)> {
    let start = events
        .iter()
        .filter(|(s, record)| *s <= sequence && record["event"]["type"] == "planets")
        .map(|(s, _)| *s)
        .last()
        .unwrap_or(i64::MIN);
    let end = events
        .iter()
        .find(|(s, record)| *s > sequence && record["event"]["type"] == "planets")
        .map_or(i64::MAX, |(s, _)| *s);
    let mut out: Vec<(i64, ShotObservation)> = Vec::new();
    for (s, record) in events {
        if *s <= start || *s >= end || record["event"]["type"] != "shot_fin" {
            continue;
        }
        let Ok(RecordedEvent::Inbound(event)) = serde_json::from_value(record.clone()) else {
            continue;
        };
        if let Ok(obs) = ShotObservation::try_from(&event)
            && !out.iter().any(|(_, o)| *o == obs)
        {
            out.push((*s, obs));
        }
    }
    out
}

/// How many of `paths` the belief recorded at `sequence` had fitted: `paths_fitted` when
/// recorded, else the paths finished before the decision minus its lag (`inferred`).
pub fn fitted_count(
    paths: &[(i64, ShotObservation)],
    sequence: i64,
    data: &Value,
    lag: &Value,
) -> (usize, &'static str) {
    if let Some(n) = data["map_belief"]["paths_fitted"].as_u64() {
        return ((n as usize).min(paths.len()), "recorded");
    }
    let before = paths.iter().filter(|(s, _)| *s < sequence).count();
    let lag = lag["paths"].as_u64().unwrap_or(0) as usize;
    (before.saturating_sub(lag), "inferred")
}

/// The belief's fog model: recorded, else the known count it used, else the default prior.
pub fn fog_model(belief: &Value) -> (FogModel, &'static str) {
    if let Ok(model) = serde_json::from_value::<FogModel>(belief["fog_model"].clone()) {
        return (model, "recorded");
    }
    match belief["residual_model"]["total_planets"].as_u64() {
        Some(n) => (FogModel::known(n as usize), "assumed"),
        None => (FogModel::DEFAULT, "assumed"),
    }
}

fn planets_of(belief: &Value) -> Vec<Planet> {
    serde_json::from_value(belief["planets"].clone()).unwrap_or_default()
}

/// The fog of the decision `data` recorded at `sequence`, with what it was built from.
pub fn decision_fog(
    events: &[(i64, Value)],
    sequence: i64,
    data: &Value,
    lag: &Value,
) -> Option<(Fog, Value)> {
    let belief = &data["map_belief"];
    if belief.is_null() {
        return None;
    }
    let paths = epoch_paths(events, sequence);
    let (fitted, fitted_source) = fitted_count(&paths, sequence, data, lag);
    let (model, model_source) = fog_model(belief);
    let observations: Vec<ShotObservation> =
        paths[..fitted].iter().map(|(_, o)| o.clone()).collect();
    let fog = unseen_fog(&planets_of(belief), &observations, model);
    let summary = fog.summary();
    let info = json!({
        "model": model,
        "model_source": model_source,
        "paths_fitted": fitted,
        "paths_fitted_source": fitted_source,
        "expected_unseen": summary.expected_unseen,
        "entropy_bits": summary.entropy_bits,
        "miss_fraction": summary.miss_fraction,
        "count_mean": summary.count_mean,
        "count_sd": summary.count_sd,
        "count_mode": summary.count_mode,
        "count_probabilities": fog.posterior.probabilities.iter().map(|p| (p * 1e4).round() / 1e4).collect::<Vec<_>>(),
        "found": fog.posterior.found,
    });
    Some((fog, info))
}

/// The fog over the battlefield at `FOG_STEP` spacing (cell centres from `FOG_STEP / 2`):
/// `risk` (probability of an unseen planet's centre within 100 units) and `pull` (root mean
/// square acceleration of the unseen planets), base64 f32 row-major.
pub fn fog_grid(fog: &Fog, info: Value) -> Value {
    let world = WorldParameters::default();
    let columns = (world.battlefield_width / FOG_STEP).ceil() as usize;
    let rows = (world.battlefield_height / FOG_STEP).ceil() as usize;
    let mut risk = Vec::with_capacity(columns * rows);
    let mut pull = Vec::with_capacity(columns * rows);
    for j in 0..rows {
        for i in 0..columns {
            let p = Vec2::new((i as f64 + 0.5) * FOG_STEP, (j as f64 + 0.5) * FOG_STEP);
            risk.push(fog.risk_at(p) as f32);
            pull.push(fog.pull_at(p) as f32);
        }
    }
    json!({
        "step": FOG_STEP,
        "columns": columns,
        "rows": rows,
        "risk": base64_f32(&risk),
        "pull": base64_f32(&pull),
        "info": info,
    })
}

/// Every decision's belief in the epoch of `paths` as `(paths fitted, planets, model)`, sorted by
/// paths fitted (later decisions win ties).
fn epoch_beliefs(
    connection: &Connection,
    capture_id: i64,
    paths: &[(i64, ShotObservation)],
    feed: &[(i64, Option<bool>)],
    range: (i64, i64),
) -> Result<Vec<(usize, Vec<Planet>, FogModel)>, String> {
    let mut statement = connection
        .prepare("SELECT sequence, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' AND sequence>?2 AND sequence<?3 ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![capture_id, range.0, range.1], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut out = Vec::new();
    for row in rows {
        let (sequence, text) = row.map_err(|error| error.to_string())?;
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let belief = &data["map_belief"];
        if belief.is_null() {
            continue;
        }
        let lag = map_lag(&data, feed, sequence);
        let (fitted, _) = fitted_count(paths, sequence, &data, &lag);
        out.push((fitted, planets_of(belief), fog_model(belief).0));
    }
    out.sort_by_key(|b| b.0);
    Ok(out)
}

/// The map epochs of a capture as `(start, end)` sequence ranges (split at `planets` events).
fn epochs(events: &[(i64, Value)]) -> Vec<(i64, i64)> {
    let mut starts: Vec<i64> = events
        .iter()
        .filter(|(_, record)| record["event"]["type"] == "planets")
        .map(|(s, _)| *s)
        .collect();
    starts.insert(0, i64::MIN);
    starts
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, starts.get(i + 1).copied().unwrap_or(i64::MAX)))
        .collect()
}

fn information_json(sequence: i64, index: usize, info: &PathInformation) -> Value {
    json!({
        "sequence": sequence,
        "index": index,
        "bits": info.bits,
        "found_bits": info.found_bits,
        "ruled_out_bits": info.ruled_out_bits,
        "entropy_bits": info.entropy_bits,
        "expected_unseen": info.expected_unseen,
        "planets": info.planets,
    })
}

/// `GET /api/fog-timeline?id=`: for every path of every map epoch, what it taught (bits, split
/// into a planet found and space ruled out) and the entropy left after it.
pub fn fog_timeline_request(
    path: &PathBuf,
    parameters: &BTreeMap<&str, &str>,
) -> Result<Value, String> {
    static CACHE: OnceLock<Mutex<HashMap<(PathBuf, i64), (i64, Value)>>> = OnceLock::new();
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
    let feed = feed_events(&connection, capture_id)?;
    let mut out = Vec::new();
    for range in epochs(&events) {
        let paths = epoch_paths(&events, range.0.saturating_add(1));
        if paths.is_empty() {
            continue;
        }
        let beliefs = epoch_beliefs(&connection, capture_id, &paths, &feed, range)?;
        let observations: Vec<ShotObservation> = paths.iter().map(|(_, o)| o.clone()).collect();
        let (start, steps) = fog_timeline(&observations, &beliefs);
        out.push(json!({
            "start": range.0.max(0),
            "start_entropy_bits": start.entropy_bits,
            "start_expected_unseen": start.expected_unseen,
            "paths": steps.iter().enumerate().map(|(k, info)| information_json(paths[k].0, k + 1, info)).collect::<Vec<_>>(),
        }));
    }
    let result = json!({"capture_id": capture_id, "epochs": out});
    cache
        .lock()
        .map_err(|_| "cache poisoned")?
        .insert(key, (last_sequence, result.clone()));
    Ok(result)
}

/// `GET /api/fog-path?id=&seq=` for a `shot_fin` event: what that path taught and where (the
/// drop of the risk, before minus after, on the fog grid).
pub fn fog_path_request(
    path: &PathBuf,
    parameters: &BTreeMap<&str, &str>,
) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let connection = open_database(path)?;
    let events = read_events_before(&connection, capture_id, i64::MAX)?;
    let paths = epoch_paths(&events, sequence);
    let Some(index) = paths.iter().position(|(s, _)| *s == sequence) else {
        return Err(
            "not a completed path of this capture (or a duplicate of an earlier one)".into(),
        );
    };
    let range = epochs(&events)
        .into_iter()
        .find(|(start, end)| *start < sequence && sequence < *end)
        .unwrap_or((i64::MIN, i64::MAX));
    let feed = feed_events(&connection, capture_id)?;
    let beliefs = epoch_beliefs(&connection, capture_id, &paths, &feed, range)?;
    let belief_at = |k: usize| {
        beliefs.iter().take_while(|b| b.0 <= k).last().map_or(
            (
                Vec::new(),
                beliefs.first().map_or(FogModel::DEFAULT, |b| b.2),
            ),
            |b| (b.1.clone(), b.2),
        )
    };
    let observations: Vec<ShotObservation> = paths.iter().map(|(_, o)| o.clone()).collect();
    let (planets_before, model_before) = belief_at(index);
    let (planets_after, model_after) = belief_at(index + 1);
    let before = unseen_fog(&planets_before, &observations[..index], model_before);
    let after = unseen_fog(&planets_after, &observations[..=index], model_after);
    let info = fog_information(&before, &after);
    let world = WorldParameters::default();
    let columns = (world.battlefield_width / FOG_STEP).ceil() as usize;
    let rows = (world.battlefield_height / FOG_STEP).ceil() as usize;
    let mut drop = Vec::with_capacity(columns * rows);
    for j in 0..rows {
        for i in 0..columns {
            let p = Vec2::new((i as f64 + 0.5) * FOG_STEP, (j as f64 + 0.5) * FOG_STEP);
            drop.push((before.risk_at(p) - after.risk_at(p)) as f32);
        }
    }
    Ok(json!({
        "sequence": sequence,
        "information": information_json(sequence, index + 1, &info),
        "planets_before": planets_before.len(),
        "planets_after": planets_after.len(),
        "footprint": {"step": FOG_STEP, "columns": columns, "rows": rows, "risk_drop": base64_f32(&drop)},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shot_fin(x0: f32, y: f32) -> Value {
        let points: Vec<Value> = (0..40)
            .map(|i| json!({"x": x0 + 5.0 * i as f32, "y": y}))
            .collect();
        json!({"direction": "inbound", "event": {"type": "shot_fin", "player_id": 1, "angle": 0.0, "velocity": 5.0, "points": points}})
    }

    fn planets_event() -> Value {
        json!({"direction": "inbound", "event": {"type": "planets", "planets": [{"x": 900.0, "y": 500.0, "radius": 30.0, "mass": 2700.0}]}})
    }

    #[test]
    fn epoch_paths_stay_in_the_decisions_map_epoch_and_skip_duplicates() {
        let events = vec![
            (1, shot_fin(100.0, 100.0)),
            (2, planets_event()),
            (3, shot_fin(100.0, 200.0)),
            (4, shot_fin(100.0, 200.0)),
            (5, shot_fin(100.0, 300.0)),
            (6, planets_event()),
            (7, shot_fin(100.0, 400.0)),
        ];
        let paths = epoch_paths(&events, 4);
        assert_eq!(paths.iter().map(|p| p.0).collect::<Vec<_>>(), vec![3, 5]);
        assert_eq!(epochs(&events).len(), 3);
    }

    #[test]
    fn fitted_paths_and_the_fog_model_are_recorded_or_marked_as_assumed() {
        let events = vec![
            (2, planets_event()),
            (3, shot_fin(100.0, 200.0)),
            (5, shot_fin(100.0, 300.0)),
            (7, shot_fin(100.0, 400.0)),
        ];
        let paths = epoch_paths(&events, 9);
        let recorded = json!({"map_belief": {"paths_fitted": 2, "planets": [], "fog_model": FogModel::known(24)}});
        assert_eq!(
            fitted_count(&paths, 9, &recorded, &Value::Null),
            (2, "recorded")
        );
        assert_eq!(
            fog_model(&recorded["map_belief"]),
            (FogModel::known(24), "recorded")
        );
        // Before P26: the paths finished before the decision minus the recorded lag.
        let old = json!({"map_belief": {"planets": [], "residual_model": {"total_planets": 22}}});
        assert_eq!(
            fitted_count(&paths, 6, &old, &json!({"paths": 1})),
            (1, "inferred")
        );
        assert_eq!(
            fog_model(&old["map_belief"]),
            (FogModel::known(22), "assumed")
        );
        assert_eq!(
            fog_model(&json!({"planets": []})),
            (FogModel::DEFAULT, "assumed")
        );
        let (fog, info) = decision_fog(&events, 9, &recorded, &Value::Null).unwrap();
        assert_eq!(info["paths_fitted"], 2);
        assert_eq!(info["model_source"], "recorded");
        assert_eq!(fog.posterior.found, 0);
        let grid = fog_grid(&fog, info);
        assert!(grid["risk"].is_string() && grid["pull"].is_string());
        assert!(decision_fog(&events, 9, &json!({"map_belief": null}), &Value::Null).is_none());
    }

    #[test]
    fn the_shot_review_applies_a_residual_correction_only_when_the_belief_had_one() {
        let events = vec![(2, planets_event()), (3, shot_fin(100.0, 200.0))];
        let without = json!({"map_belief": {"planets": [], "residual_correction": null, "residual_model": {"scale": 0.5, "length": 600.0, "white": 1e-6}}});
        assert!(crate::review::used_correction(&events, &[], 9, &without).is_none());
        let old = json!({"map_belief": {"planets": []}});
        assert!(crate::review::used_correction(&events, &[], 9, &old).is_none());
    }
}
