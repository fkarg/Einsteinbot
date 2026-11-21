//! Read-only local inspector for recorded targeting decisions.
use einsteinbot::field_hypothesis::FieldHypothesis;
use einsteinbot::local_field::LocalField;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_shot, simulate_targeting_shot,
    simulate_targeting_shot_with_hypothesis, simulate_targeting_shot_with_local_field,
};
use einsteinbot::strategy::{BotState, MapBelief, ShotChoice, TargetingPolicy, TargetingSettings};
use nalgebra::DMatrix;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

#[path = "map_viewer/degrade.rs"]
mod degrade;
#[path = "map_viewer/review.rs"]
mod review;
#[path = "map_viewer/scenario.rs"]
mod scenario;

const PAGE: &str = include_str!("../../web/map_viewer.html");
const SCRIPT: &str = include_str!("../../web/map_viewer.js");
const DEFAULT_PORT: u16 = 8765;
const PLAYER_RADIUS: f64 = 4.0;
const FIELD_GRID_STEP: usize = 3;
const MAX_FIELD_SAMPLES: usize = 300_000;
/// Largest center distance at which an estimated planet may be paired with a disclosed one.
const MATCH_DISTANCE: f64 = 80.0;

fn main() {
    if let Err(error) = run() {
        eprintln!("map_viewer failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut db_path = PathBuf::from("einsteinbot.sqlite3");
    let mut port = DEFAULT_PORT;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--db-path" => {
                db_path = arguments.next().ok_or("--db-path requires a path")?.into();
            }
            "--port" => {
                port = arguments.next().ok_or("--port requires a value")?.parse()?;
            }
            "--help" | "-h" => {
                println!("Usage: map_viewer [--db-path PATH] [--port PORT]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    println!("Map viewer: http://127.0.0.1:{port} (read-only)");
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // One thread per request: a slow review must not block snapshots.
                let db_path = db_path.clone();
                std::thread::spawn(move || {
                    if let Err(error) = handle(stream, &db_path) {
                        eprintln!("map_viewer request failed: {error}");
                    }
                });
            }
            Err(error) => eprintln!("map_viewer connection failed: {error}"),
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, db_path: &PathBuf) -> Result<(), Box<dyn Error>> {
    let mut request = [0u8; 4096];
    let read = stream.read(&mut request)?;
    let first_line = std::str::from_utf8(&request[..read])?
        .lines()
        .next()
        .ok_or("empty HTTP request")?;
    let mut pieces = first_line.split_whitespace();
    let method = pieces.next().ok_or("missing HTTP method")?;
    let target = pieces.next().ok_or("missing request target")?;
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", "GET only");
    }
    if target == "/" || target == "/index.html" {
        return respond(&mut stream, 200, "text/html; charset=utf-8", PAGE);
    }
    if target == "/map_viewer.js" {
        return respond(&mut stream, 200, "text/javascript; charset=utf-8", SCRIPT);
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let parameters = parse_query(query);
    let result = match path {
        "/api/captures" => list_captures(db_path),
        "/api/overview" => capture_overview(db_path),
        "/api/degrade" => degrade::degrade(db_path, &parameters),
        "/api/shot-review" => review::shot_review(db_path, &parameters),
        "/api/predictive" => review::predictive_error(db_path, &parameters),
        "/api/decisions" => {
            required_id(&parameters, "id").and_then(|id| list_decisions(db_path, id))
        }
        "/api/snapshot" => selected_snapshot(db_path, &parameters),
        "/api/timeline" => capture_timeline(db_path, &parameters),
        "/api/simulate" => simulate_request(db_path, &parameters),
        "/api/compare" => compare_request(db_path, &parameters),
        "/api/physics-audit" => physics_audit(db_path, &parameters),
        _ => return respond(&mut stream, 404, "text/plain", "not found"),
    };
    match result {
        Ok(value) => respond(
            &mut stream,
            200,
            "application/json; charset=utf-8",
            &serde_json::to_string(&value)?,
        ),
        Err(error) => respond(
            &mut stream,
            400,
            "application/json; charset=utf-8",
            &json!({"error": error}).to_string(),
        ),
    }
}

fn respond(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &str,
) -> Result<(), Box<dyn Error>> {
    let label = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {label}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )?;
    Ok(())
}

fn parse_query(query: &str) -> BTreeMap<&str, &str> {
    query
        .split('&')
        .filter_map(|part| part.split_once('='))
        .collect()
}

fn required_id(parameters: &BTreeMap<&str, &str>, key: &str) -> Result<i64, String> {
    parameters
        .get(key)
        .ok_or_else(|| format!("missing {key}"))?
        .parse()
        .map_err(|_| format!("invalid {key}"))
}

fn required_number(parameters: &BTreeMap<&str, &str>, key: &str) -> Result<f64, String> {
    let number = parameters
        .get(key)
        .ok_or_else(|| format!("missing {key}"))?
        .parse::<f64>()
        .map_err(|_| format!("invalid {key}"))?;
    number
        .is_finite()
        .then_some(number)
        .ok_or_else(|| format!("nonfinite {key}"))
}

fn open_database(path: &PathBuf) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| error.to_string())
}

fn list_captures(path: &PathBuf) -> Result<Value, String> {
    let connection = open_database(path)?;
    let mut statement = connection
        .prepare("SELECT id, started_at_unix_ms, ended_at_unix_ms, metadata_json FROM captures ORDER BY id DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let metadata: String = row.get(3)?;
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "started_at_unix_ms": row.get::<_, i64>(1)?,
                "ended_at_unix_ms": row.get::<_, Option<i64>>(2)?,
                "metadata": serde_json::from_str::<Value>(&metadata).unwrap_or(Value::Null),
            }))
        })
        .map_err(|error| error.to_string())?;
    let captures = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(json!({"captures": captures}))
}

/// One summary row per capture for the landing table. Only each capture's last decision is parsed.
fn capture_overview(path: &PathBuf) -> Result<Value, String> {
    let connection = open_database(path)?;
    let mut shots: BTreeMap<i64, i64> = BTreeMap::new();
    let mut statement = connection
        .prepare("SELECT capture_id, COUNT(*) FROM events WHERE event_type='shot_fin' GROUP BY capture_id")
        .map_err(|error| error.to_string())?;
    for row in statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .map_err(|error| error.to_string())?
    {
        let (id, count) = row.map_err(|error| error.to_string())?;
        shots.insert(id, count);
    }
    let mut captures = Vec::new();
    let mut statement = connection
        .prepare("SELECT id, started_at_unix_ms, ended_at_unix_ms, metadata_json FROM captures ORDER BY id DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    for row in rows {
        let (id, started, ended, metadata) = row.map_err(|error| error.to_string())?;
        let metadata: Value = serde_json::from_str(&metadata).unwrap_or(Value::Null);
        let decisions: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM diagnostics WHERE capture_id=?1 AND kind='target_decision'",
                [id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let last = connection
            .query_row(
                "SELECT sequence, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence DESC LIMIT 1",
                [id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .and_then(|(sequence, text)| {
                let data: Value = serde_json::from_str(&text).ok()?;
                let belief = &data["map_belief"];
                let accuracy = disclosed_maps(&connection, id)
                    .ok()
                    .and_then(|disclosed| decision_accuracy(&disclosed, sequence, belief));
                Some(json!({
                    "sequence": sequence,
                    "stage": data["scenario"]["stage"],
                    "planets": belief["planets"].as_array().map(Vec::len),
                    "fit_rmse": belief["model_position_rmse"],
                    "fit_converged": belief["fit_converged"],
                    "accuracy": accuracy,
                }))
            });
        captures.push(json!({
            "id": id,
            "started_at_unix_ms": started,
            "ended_at_unix_ms": ended,
            "kind": if metadata["host"] == "synthetic-scenario" { "scenario" } else { "live" },
            "name": metadata["client_name"],
            "revision": metadata["build"]["git_revision"],
            "shots": shots.get(&id).copied().unwrap_or(0),
            "decisions": decisions,
            "last": last,
        }));
    }
    // Synthetic scenarios are regenerated per code revision; only the newest of each name is current.
    let mut newest: BTreeMap<String, i64> = BTreeMap::new();
    for capture in &captures {
        if capture["kind"] == "scenario" {
            let (name, id) = (
                capture["name"].to_string(),
                capture["id"].as_i64().unwrap_or(0),
            );
            newest
                .entry(name)
                .and_modify(|best| *best = (*best).max(id))
                .or_insert(id);
        }
    }
    for capture in &mut captures {
        capture["latest_revision"] = json!(
            capture["kind"] != "scenario"
                || newest.get(&capture["name"].to_string()) == capture["id"].as_i64().as_ref()
        );
    }
    Ok(json!({"captures": captures}))
}

fn list_decisions(path: &PathBuf, capture_id: i64) -> Result<Value, String> {
    let connection = open_database(path)?;
    let mut statement = connection
        .prepare("SELECT sequence, timestamp_unix_ms, bot_name, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let disclosed = disclosed_maps(&connection, capture_id)?;
    let rows = statement
        .query_map([capture_id], |row| {
            let data: String = row.get(3)?;
            let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let belief = &data["map_belief"];
            let sequence: i64 = row.get(0)?;
            let accuracy = decision_accuracy(&disclosed, sequence, belief);
            Ok(json!({
                "sequence": sequence,
                "timestamp_unix_ms": row.get::<_, i64>(1)?,
                "bot_name": row.get::<_, String>(2)?,
                "own_id": data["bot_state"]["own_id"],
                "accuracy": accuracy,
                "energy": data["energy"],
                "choice": data["choice"],
                "scenario": data["scenario"],
                "map_updating": data["map_updating"],
                "map_planets": belief["planets"].as_array().map_or(0, Vec::len),
                "fit_rmse": belief["model_position_rmse"],
                "fit_converged": belief["fit_converged"],
                "covariance": belief["covariance"].is_array(),
            }))
        })
        .map_err(|error| error.to_string())?;
    let decisions = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(json!({"capture_id": capture_id, "decisions": decisions}))
}

/// Every disclosed-planets message of a capture as `(sequence, planets)`, in sequence order.
fn disclosed_maps(
    connection: &Connection,
    capture_id: i64,
) -> Result<Vec<(i64, Vec<Planet>)>, String> {
    let mut statement = connection
        .prepare("SELECT sequence, event_json FROM events WHERE capture_id=?1 AND event_type='planets' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([capture_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut maps = Vec::new();
    for row in rows {
        let (sequence, text) = row.map_err(|error| error.to_string())?;
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Ok(planets) = serde_json::from_value(value["event"]["planets"].clone()) {
            maps.push((sequence, planets));
        }
    }
    Ok(maps)
}

fn capture_timeline(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let connection = open_database(path)?;
    let roles = scenario::ShotRoles::load(&connection, capture_id)?;
    let mut statement = connection.prepare("SELECT sequence, timestamp_unix_ms, event_type, event_json FROM events WHERE capture_id=?1 ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let events = statement.query_map([capture_id], |row| {
        let sequence: i64 = row.get(0)?;
        let event_type: String = row.get(2)?;
        let event: String = row.get(3)?;
        let event: Value = serde_json::from_str(&event).unwrap_or(Value::Null);
        let e = &event["event"];
        let detail = match event_type.as_str() {
            "planets" => format!("{} disclosed planets", e["planets"].as_array().map_or(0, Vec::len)),
            "shot_fin" => format!("player {} shot ended · {} points", e["player_id"], e["points"].as_array().map_or(0, Vec::len)),
            "player_position" => format!("player {} at ({:.0}, {:.0})", e["player_id"], e["x"].as_f64().unwrap_or(0.0), e["y"].as_f64().unwrap_or(0.0)),
            "shot_begin" => format!("player {} fired v{} at {}°", e["player_id"], e["velocity"], e["angle"]),
            other => other.to_owned(),
        };
        let detail = if event_type == "shot_fin" {
            roles.label(sequence, e).map_or(detail.clone(), |label| format!("{label} · {detail}"))
        } else {
            detail
        };
        Ok(json!({"sequence":sequence,"timestamp_unix_ms":row.get::<_,i64>(1)?,"kind":event_type,"detail":detail}))
    }).map_err(|error| error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
    let mut statement = connection.prepare("SELECT sequence, timestamp_unix_ms, bot_name, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let decisions = statement.query_map([capture_id], |row| {
        let sequence: i64 = row.get(0)?; let timestamp: i64 = row.get(1)?; let bot: String = row.get(2)?;
        let data: String = row.get(3)?; let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let choice = &data["choice"];
        let stage = data["scenario"]["stage"].as_str();
        let detail = stage.map_or_else(
            || format!("{bot} chose v{} at {}° ({})", choice["velocity"], choice["angle_degrees"], choice["reason"].as_str().unwrap_or("no shot")),
            |stage| format!("{stage} · {} estimated planets", data["map_belief"]["planets"].as_array().map_or(0, Vec::len)),
        );
        Ok(json!({"sequence":sequence,"timestamp_unix_ms":timestamp,"kind":"decision","detail":detail}))
    }).map_err(|error| error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
    let mut timeline = events;
    timeline.extend(decisions);
    timeline.sort_by_key(|event| event["sequence"].as_i64().unwrap_or(0));
    Ok(json!({"capture_id":capture_id,"timeline":timeline}))
}

fn read_decision(connection: &Connection, capture_id: i64, sequence: i64) -> Result<Value, String> {
    connection
        .query_row(
            "SELECT data_json FROM diagnostics WHERE capture_id=?1 AND sequence=?2 AND kind='target_decision'",
            params![capture_id, sequence],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| error.to_string())
        .and_then(|text| serde_json::from_str(&text).map_err(|error| error.to_string()))
}

fn read_events_before(
    connection: &Connection,
    capture_id: i64,
    sequence: i64,
) -> Result<Vec<(i64, Value)>, String> {
    let mut statement = connection
        .prepare("SELECT sequence, event_json FROM events WHERE capture_id=?1 AND sequence<?2 AND event_type IN ('own_id','player_position','player_leave','shot_begin','shot_fin','planets') ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![capture_id, sequence], |row| {
            let text: String = row.get(1)?;
            Ok((
                row.get(0)?,
                serde_json::from_str::<Value>(&text).unwrap_or(Value::Null),
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

/// Where every player was last seen, who this bot is, and which shots have begun but not finished,
/// as of the end of `events` (so the map can show them between decision snapshots).
fn world_state(events: &[(i64, Value)], decision_own_id: Option<u64>) -> Value {
    let mut players: BTreeMap<u64, Value> = BTreeMap::new();
    let mut in_flight: BTreeMap<u64, Value> = BTreeMap::new();
    let mut own_id = None;
    // Distinct earlier positions of each player, so a move reads as a move rather than a respawn.
    let mut earlier: BTreeMap<u64, Vec<Value>> = BTreeMap::new();
    for (_, record) in events {
        let event = &record["event"];
        if event["type"] == "planets" {
            // A new map respawns everyone; earlier positions belong to the old map.
            earlier.clear();
            continue;
        }
        let Some(id) = event["player_id"].as_u64() else {
            continue;
        };
        match event["type"].as_str() {
            Some("player_position") => {
                let moved = players.get(&id).is_none_or(|old| {
                    (old["x"].as_f64().unwrap_or(0.0) - event["x"].as_f64().unwrap_or(0.0)).hypot(
                        old["y"].as_f64().unwrap_or(0.0) - event["y"].as_f64().unwrap_or(0.0),
                    ) > 0.5
                });
                if moved && let Some(old) = players.get(&id) {
                    let spots = earlier.entry(id).or_default();
                    let near = |a: &Value, b: &Value| {
                        (a["x"].as_f64().unwrap_or(0.0) - b["x"].as_f64().unwrap_or(0.0))
                            .hypot(a["y"].as_f64().unwrap_or(0.0) - b["y"].as_f64().unwrap_or(0.0))
                            <= 0.5
                    };
                    spots.retain(|spot| !near(spot, old));
                    spots.push(old.clone());
                    if spots.len() > 3 {
                        spots.remove(0);
                    }
                }
                players.insert(id, json!({"x": event["x"], "y": event["y"]}));
            }
            Some("player_leave") => {
                players.remove(&id);
                earlier.remove(&id);
                in_flight.remove(&id);
            }
            Some("shot_begin") => {
                in_flight.insert(
                    id,
                    json!({"player_id": id, "velocity": event["velocity"], "angle_degrees": event["angle"]}),
                );
            }
            Some("shot_fin") => {
                in_flight.remove(&id);
            }
            Some("own_id") => own_id = Some(id),
            _ => {}
        }
    }
    json!({
        "players": players.iter().map(|(id, position)| (id.to_string(), position.clone())).collect::<BTreeMap<_, _>>(),
        "own_id": decision_own_id.or(own_id),
        "ghosts": earlier
            .into_iter()
            .map(|(id, positions)| {
                let here = players.get(&id).cloned().unwrap_or(Value::Null);
                let ghosts = positions
                    .into_iter()
                    .filter(|spot| {
                        (spot["x"].as_f64().unwrap_or(0.0) - here["x"].as_f64().unwrap_or(0.0))
                            .hypot(spot["y"].as_f64().unwrap_or(0.0) - here["y"].as_f64().unwrap_or(0.0))
                            > 0.5
                    })
                    .collect::<Vec<_>>();
                (id.to_string(), ghosts)
            })
            .collect::<BTreeMap<_, _>>(),
        "in_flight": in_flight.into_values().collect::<Vec<_>>(),
    })
}

fn latest_actual_planets(events: &[(i64, Value)]) -> Option<Vec<Planet>> {
    events.iter().rev().find_map(|(_, stored)| {
        let event = &stored["event"];
        (event["type"] == "planets")
            .then(|| serde_json::from_value(event["planets"].clone()).ok())
            .flatten()
    })
}

/// Pairs estimated with disclosed planets one-to-one, closest pair first, within `MATCH_DISTANCE`.
/// Returns `(estimated index, actual index, center distance)`.
fn match_planets(estimated: &[Planet], actual: &[Planet]) -> Vec<(usize, usize, f64)> {
    let mut candidates = Vec::new();
    for (k, estimate) in estimated.iter().enumerate() {
        for (i, truth) in actual.iter().enumerate() {
            let distance = (estimate.x - truth.x).hypot(estimate.y - truth.y);
            if distance <= MATCH_DISTANCE {
                candidates.push((k, i, distance));
            }
        }
    }
    candidates.sort_by(|a, b| a.2.total_cmp(&b.2));
    let (mut used_estimated, mut used_actual) =
        (vec![false; estimated.len()], vec![false; actual.len()]);
    let mut pairs = Vec::new();
    for (k, i, distance) in candidates {
        if !used_estimated[k] && !used_actual[i] {
            used_estimated[k] = true;
            used_actual[i] = true;
            pairs.push((k, i, distance));
        }
    }
    pairs.sort_by_key(|pair| pair.1);
    pairs
}

/// How well a decision's belief matches the latest disclosed map before it, if there is both.
fn decision_accuracy(
    disclosed: &[(i64, Vec<Planet>)],
    sequence: i64,
    belief: &Value,
) -> Option<Value> {
    let (_, actual) = disclosed
        .iter()
        .rev()
        .find(|(planets_sequence, _)| *planets_sequence < sequence)?;
    let estimated: Vec<Planet> = serde_json::from_value(belief["planets"].clone()).ok()?;
    let pairs = match_planets(&estimated, actual);
    let mut errors: Vec<f64> = pairs.iter().map(|pair| pair.2).collect();
    errors.sort_by(f64::total_cmp);
    Some(json!({
        "actual_planets": actual.len(),
        "matched_planets": pairs.len(),
        "missing_planets": actual.len() - pairs.len(),
        "median_center_error": median(&errors),
    }))
}

/// Median of an ascending slice; `None` when empty.
fn median(sorted: &[f64]) -> Option<f64> {
    let middle = sorted.len() / 2;
    match sorted.len() {
        0 => None,
        length if length % 2 == 1 => Some(sorted[middle]),
        _ => Some((sorted[middle - 1] + sorted[middle]) / 2.0),
    }
}

fn planet_match_json(estimated: &[Planet], actual: &[Planet]) -> Value {
    let pairs = match_planets(estimated, actual);
    let missing_actual = (0..actual.len())
        .filter(|i| pairs.iter().all(|pair| pair.1 != *i))
        .collect::<Vec<_>>();
    let unmatched_estimated = (0..estimated.len())
        .filter(|k| pairs.iter().all(|pair| pair.0 != *k))
        .collect::<Vec<_>>();
    json!({
        "threshold": MATCH_DISTANCE,
        "pairs": pairs.iter().map(|&(k, i, distance)| json!({
            "estimated": k,
            "actual": i,
            "center_error": distance,
            "radius_error": estimated[k].radius - actual[i].radius,
            "mass_ratio": estimated[k].mass / actual[i].mass,
        })).collect::<Vec<_>>(),
        "missing_actual": missing_actual,
        "unmatched_estimated": unmatched_estimated,
    })
}

fn selected_snapshot(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let connection = open_database(path)?;
    let selection = parameters.get("seq").copied().unwrap_or("none");
    let is_decision = selection.parse::<i64>().is_ok();
    let sequence = if is_decision {
        selection
            .parse::<i64>()
            .map_err(|_| "invalid seq".to_owned())?
    } else if let Some(event_seq) = selection.strip_prefix("event:") {
        event_seq
            .parse::<i64>()
            .map_err(|_| "invalid event sequence".to_owned())?
    } else {
        connection
            .query_row(
                "SELECT COALESCE(MAX(sequence),0) FROM events WHERE capture_id=?1",
                [capture_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?
    };
    let (data, diagnostic_sequence) = if is_decision {
        (
            read_decision(&connection, capture_id, sequence)?,
            Some(sequence),
        )
    } else {
        connection.query_row("SELECT sequence,data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' AND sequence<=?2 ORDER BY sequence DESC LIMIT 1", params![capture_id,sequence], |row| Ok((row.get::<_,i64>(0)?,row.get::<_,String>(1)?)))
            .ok().and_then(|(seq,text)| serde_json::from_str(&text).ok().map(|data|(data,Some(seq)))).unwrap_or((Value::Null,None))
    };
    let events = read_events_before(&connection, capture_id, sequence.saturating_add(1))?;
    let actual_planets = latest_actual_planets(&events);
    let display_field = parameters.get("field").copied().unwrap_or("delta");
    if ![
        "delta",
        "total",
        "physical",
        "residual",
        "oracle",
        "hypothesis_source",
        "hypothesis_residual",
        "hypothesis_total",
        "hypothesis_delta",
    ]
    .contains(&display_field)
    {
        return Err("unknown vector field".to_owned());
    }
    let grid_step = parameters.get("step").map_or(Ok(FIELD_GRID_STEP), |step| {
        step.parse::<usize>()
            .map_err(|_| "invalid grid step".to_owned())
    })?;
    if ![1, 2, 3, 5, 10, 25].contains(&grid_step) {
        return Err("grid step must be 1, 2, 3, 5, 10, or 25 world units".to_owned());
    }
    let world = WorldParameters::default();
    let view = Viewport {
        x: optional_finite(&parameters, "x", 0.0)?,
        y: optional_finite(&parameters, "y", 0.0)?,
        width: optional_finite(&parameters, "width", world.battlefield_width)?,
        height: optional_finite(&parameters, "height", world.battlefield_height)?,
    };
    if view.x < 0.0
        || view.y < 0.0
        || view.width <= 0.0
        || view.height <= 0.0
        || view.x + view.width > world.battlefield_width
        || view.y + view.height > world.battlefield_height
    {
        return Err("viewport must fit inside the battlefield".to_owned());
    }
    let start_x = (view.x as usize / grid_step) * grid_step;
    let start_y = (view.y as usize / grid_step) * grid_step;
    let end_x = (view.x + view.width).ceil() as usize;
    let end_y = (view.y + view.height).ceil() as usize;
    let columns = (end_x.saturating_sub(start_x) / grid_step) + 1;
    let rows = (end_y.saturating_sub(start_y) / grid_step) + 1;
    let sample_count = columns.saturating_mul(rows);
    if sample_count > MAX_FIELD_SAMPLES {
        return Err(format!(
            "viewport at {grid_step}-unit spacing requests {sample_count} samples; zoom in or choose a coarser grid"
        ));
    }
    let field_grid = make_field_grid(
        &data,
        actual_planets.as_deref(),
        grid_step,
        view,
        display_field,
    )?;
    let roles = scenario::ShotRoles::load(&connection, capture_id)?;
    let selected_shot = selection.strip_prefix("event:").and_then(|_| {
        events
                .iter()
                .find(|(event_sequence, record)| {
                    *event_sequence == sequence && record["event"]["type"] == "shot_fin"
                })
                .map(|(event_sequence, record)| {
                    json!({"sequence":event_sequence,"event":record["event"],"label":roles.label(*event_sequence, &record["event"])})
                })
    });
    let trace_map = decode_belief(&data["map_belief"])
        .ok()
        .flatten()
        .map(|belief| belief.map);
    let map_epoch_start = events
        .iter()
        .rev()
        .find(|(_, record)| record["event"]["type"] == "planets")
        .map_or(0, |(sequence, _)| *sequence);
    let trajectories = events
        .iter()
        .filter(|(_, record)| record["event"]["type"] == "shot_fin")
        .rev()
        .take(60)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(sequence, record)| {
            let points = serde_json::from_value::<Vec<Point>>(record["event"]["points"].clone())
                .unwrap_or_default();
            let residual_samples = trace_map
                .as_ref()
                .map(|map| {
                    trajectory_residual_samples_in_epoch(*sequence, map_epoch_start, &points, map)
                })
                .unwrap_or_default();
            json!({
                "sequence": sequence,
                "record": record,
                "residual_samples": residual_samples,
                "label": roles.label(*sequence, &record["event"]),
            })
        })
        .collect::<Vec<_>>();
    // Compare only a belief with the disclosed map it was built against: a `Planets` message newer
    // than the belief's snapshot starts a new map epoch the belief knows nothing about.
    let latest_planets_sequence = events
        .iter()
        .rev()
        .find(|(_, record)| record["event"]["type"] == "planets")
        .map(|(sequence, _)| *sequence);
    let planet_match = actual_planets
        .as_deref()
        .filter(|_| {
            is_decision
                || diagnostic_sequence.is_some_and(|belief_sequence| {
                    latest_planets_sequence.is_none_or(|planets| planets < belief_sequence)
                })
        })
        .and_then(|actual| {
            let estimated: Vec<Planet> =
                serde_json::from_value(data["map_belief"]["planets"].clone()).ok()?;
            Some(planet_match_json(&estimated, actual))
        });
    Ok(json!({
        "capture_id": capture_id,
        "sequence": sequence,
        "diagnostic": data,
        "diagnostic_sequence": diagnostic_sequence,
        "selected_shot": selected_shot,
        "selection_kind": if is_decision {"decision"} else if selection.starts_with("event:") {"timeline_event"} else {"latest_capture_state"},
        "events": trajectories,
        "actual_planets": actual_planets,
        "planet_match": planet_match,
        "world": world_state(&events, data["bot_state"]["own_id"].as_u64()),
        "field_grid": field_grid,
        "view": {"x":view.x,"y":view.y,"width":view.width,"height":view.height},
    }))
}

#[derive(Clone, Copy)]
struct Viewport {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn optional_finite(
    parameters: &BTreeMap<&str, &str>,
    key: &str,
    default: f64,
) -> Result<f64, String> {
    let Some(value) = parameters.get(key) else {
        return Ok(default);
    };
    let value = value.parse::<f64>().map_err(|_| format!("invalid {key}"))?;
    value
        .is_finite()
        .then_some(value)
        .ok_or_else(|| format!("nonfinite {key}"))
}

fn decode_belief(value: &Value) -> Result<Option<MapBelief>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let planets: Vec<Planet> = serde_json::from_value(value["planets"].clone())
        .map_err(|error| format!("invalid captured planets: {error}"))?;
    let map = PlanetMap { planets };
    let mut belief = MapBelief::new(map, None);
    if let Some(samples) = value.get("local_field") {
        belief.local_field = serde_json::from_value::<LocalField>(samples.clone())
            .map_err(|error| format!("invalid captured local field: {error}"))?;
    }
    if let Some(hypothesis) = value
        .get("field_hypothesis")
        .filter(|value| !value.is_null())
    {
        belief.field_hypothesis = Some(
            serde_json::from_value::<FieldHypothesis>(hypothesis.clone())
                .map_err(|error| format!("invalid captured field hypothesis: {error}"))?,
        );
    }
    if let Some(field) = value
        .get("hypothesis_local_field")
        .filter(|value| !value.is_null())
    {
        belief.hypothesis_local_field = Some(
            serde_json::from_value::<LocalField>(field.clone())
                .map_err(|error| format!("invalid hypothesis local field: {error}"))?,
        );
    }
    if let Some(covariance) = value.get("covariance").and_then(Value::as_array) {
        let rows = covariance.len();
        let columns = covariance
            .first()
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if rows > 0
            && columns > 0
            && rows == columns
            && covariance
                .iter()
                .all(|row| row.as_array().is_some_and(|row| row.len() == columns))
        {
            let matrix = DMatrix::from_fn(rows, columns, |row, column| {
                covariance[row][column].as_f64().unwrap_or(f64::NAN)
            });
            if matrix.iter().all(|value| value.is_finite()) {
                belief.parameter_covariance = Some(matrix);
            }
        }
    }
    belief.model_position_rmse = value["model_position_rmse"].as_f64();
    belief.fit_converged = value["fit_converged"].as_bool();
    belief.contact_points = serde_json::from_value(
        value
            .get("contact_points")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|error| error.to_string())?;
    belief.planet_contact_counts = serde_json::from_value(
        value
            .get("planet_contact_counts")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|error| error.to_string())?;
    belief.uncertain_associations = value["uncertain_associations"].as_u64().unwrap_or(0) as usize;
    Ok(Some(belief))
}

fn make_field_grid(
    data: &Value,
    actual: Option<&[Planet]>,
    grid_step: usize,
    view: Viewport,
    display_field: &str,
) -> Result<Value, String> {
    let belief = decode_belief(&data["map_belief"])?;
    let covariance = belief
        .as_ref()
        .and_then(|belief| belief.parameter_covariance.as_ref());
    let actual_map = actual.map(|planets| PlanetMap {
        planets: planets.to_vec(),
    });
    let world = WorldParameters::default();
    let mut samples = Vec::new();
    let start_x = (view.x as usize / grid_step) * grid_step;
    let start_y = (view.y as usize / grid_step) * grid_step;
    let end_x = (view.x + view.width).ceil() as usize;
    let end_y = (view.y + view.height).ceil() as usize;
    for y in (start_y..=end_y).step_by(grid_step) {
        for x in (start_x..=end_x).step_by(grid_step) {
            let position = Vec2::new(x as f64, y as f64);
            let Some(belief) = &belief else {
                let oracle = actual_map
                    .as_ref()
                    .and_then(|map| map.acceleration(position).ok());
                let selected = (display_field == "oracle").then_some(oracle).flatten();
                samples.push(json!([
                    x,
                    y,
                    selected.map(|v| v.x),
                    selected.map(|v| v.y),
                    Option::<f64>::None,
                    Option::<f64>::None,
                    Option::<f64>::None,
                    Option::<f64>::None,
                    Option::<f64>::None,
                    Option::<f64>::None,
                    oracle.map(|v| v.x),
                    oracle.map(|v| v.y),
                    Option::<f64>::None,
                    false,
                    Option::<f64>::None
                ]));
                continue;
            };
            let physical = belief.map.acceleration(position).ok();
            let residual_observed = belief.local_field.residual(position);
            let has_residual = residual_observed.is_some();
            let residual = residual_observed.unwrap_or_default();
            let gravity =
                physical.map(|physical| (physical.x + residual.x, physical.y + residual.y));
            let actual_vector = actual_map
                .as_ref()
                .and_then(|map| map.acceleration(position).ok());
            let delta = gravity
                .zip(actual_vector)
                .map(|(estimate, oracle)| (estimate.0 - oracle.x, estimate.1 - oracle.y));
            let wants_source = matches!(
                display_field,
                "hypothesis_source" | "hypothesis_total" | "hypothesis_delta"
            );
            let wants_hypothesis_residual = matches!(
                display_field,
                "hypothesis_residual" | "hypothesis_total" | "hypothesis_delta"
            );
            let hypothesis_source = wants_source
                .then(|| {
                    belief
                        .field_hypothesis
                        .as_ref()
                        .and_then(|hypothesis| hypothesis.acceleration(position))
                })
                .flatten();
            let hypothesis_residual = wants_hypothesis_residual
                .then(|| {
                    belief
                        .hypothesis_local_field
                        .as_ref()
                        .and_then(|field| field.residual(position))
                })
                .flatten();
            let hypothesis_total = physical.zip(hypothesis_source).map(|(physical, source)| {
                let residual = hypothesis_residual.unwrap_or_default();
                (
                    physical.x + source.x + residual.x,
                    physical.y + source.y + residual.y,
                )
            });
            let hypothesis_delta = hypothesis_total
                .zip(actual_vector)
                .map(|(estimate, oracle)| (estimate.0 - oracle.x, estimate.1 - oracle.y));
            let sigma = covariance
                .and_then(|matrix| belief.map.acceleration_covariance(position, matrix).ok())
                .map(|covariance| (covariance.xx + covariance.yy).max(0.0).sqrt());
            let selected = match display_field {
                "delta" => delta,
                "total" => gravity,
                "physical" => physical.map(|v| (v.x, v.y)),
                "residual" => residual_observed.map(|v| (v.x, v.y)),
                "oracle" => actual_vector.map(|v| (v.x, v.y)),
                "hypothesis_source" => hypothesis_source.map(|v| (v.x, v.y)),
                "hypothesis_residual" => hypothesis_residual.map(|v| (v.x, v.y)),
                "hypothesis_total" => hypothesis_total,
                "hypothesis_delta" => hypothesis_delta,
                _ => unreachable!("display field is validated by selected_snapshot"),
            };
            samples.push(json!([
                x,
                y,
                selected.map(|v| v.0),
                selected.map(|v| v.1),
                gravity.map(|v| v.0),
                gravity.map(|v| v.1),
                physical.map(|v| v.x),
                physical.map(|v| v.y),
                residual.x,
                residual.y,
                actual_vector.map(|v| v.x),
                actual_vector.map(|v| v.y),
                sigma,
                has_residual,
                belief.model_disagreement_at(position)
            ]));
        }
    }
    Ok(json!({
        "samples": samples,
        "width": world.battlefield_width,
        "height": world.battlefield_height,
        "step": grid_step,
        "origin_x": start_x,
        "origin_y": start_y,
        "columns": (end_x.saturating_sub(start_x) / grid_step) + 1,
        "rows": (end_y.saturating_sub(start_y) / grid_step) + 1,
        "has_covariance": covariance.is_some(),
        "has_belief": belief.is_some(),
    }))
}

fn trajectory_residual_samples(points: &[Point], map: &PlanetMap) -> Vec<[f64; 4]> {
    einsteinbot::simulation::reconstruct_acceleration(points)
        .into_iter()
        .filter_map(|(position, observed)| {
            let predicted = map.acceleration(position).ok()?;
            Some([
                position.x,
                position.y,
                observed.x - predicted.x,
                observed.y - predicted.y,
            ])
        })
        .collect()
}

fn trajectory_residual_samples_in_epoch(
    sequence: i64,
    map_epoch_start: i64,
    points: &[Point],
    map: &PlanetMap,
) -> Vec<[f64; 4]> {
    if sequence > map_epoch_start {
        trajectory_residual_samples(points, map)
    } else {
        Vec::new()
    }
}

fn simulate_request(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let velocity = required_number(parameters, "velocity")?;
    let angle = required_number(parameters, "angle")?;
    if !(2.0..=50.0).contains(&velocity) || !(-180.0..=180.0).contains(&angle) {
        return Err("velocity must be 2..50 and angle -180..180".to_owned());
    }
    let model = parameters.get("model").copied().unwrap_or("belief");
    let connection = open_database(path)?;
    let data = read_decision(&connection, capture_id, sequence)?;
    let belief = decode_belief(&data["map_belief"])?;
    let bot_state = BotState::from_diagnostic_snapshot(&data["bot_state"])
        .map_err(|error| error.to_string())?;
    let own_id = bot_state
        .own_id()
        .ok_or("captured decision has no own player id")?;
    let start = bot_state
        .player_positions()
        .get(&own_id)
        .cloned()
        .ok_or("captured decision has no own player position")?;
    let players = bot_state
        .player_positions()
        .iter()
        .map(|(&id, point)| (id, Vec2::new(f64::from(point.x), f64::from(point.y))))
        .collect::<Vec<_>>();
    let actual = latest_actual_planets(&read_events_before(&connection, capture_id, sequence)?);
    let map = match model {
        "actual" => PlanetMap {
            planets: actual.ok_or("this capture has no disclosed planet map at this decision")?,
        },
        "belief" | "hypothesis" => belief
            .as_ref()
            .map(|belief| belief.map.clone())
            .unwrap_or(PlanetMap { planets: vec![] }),
        _ => return Err("model must be belief, hypothesis, or actual".to_owned()),
    };
    let shot = ShotObservation {
        player_id: own_id,
        angle_degrees: angle,
        velocity,
        points: vec![start],
    };
    let result = match model {
        "hypothesis" => {
            let belief = belief.as_ref().ok_or("belief is unavailable")?;
            let hypothesis = belief
                .field_hypothesis
                .as_ref()
                .ok_or("field hypothesis is unavailable")?;
            let field = belief
                .hypothesis_local_field
                .as_ref()
                .ok_or("hypothesis local field is unavailable")?;
            simulate_targeting_shot_with_hypothesis(
                &map,
                field,
                hypothesis,
                &shot,
                WorldParameters::default(),
                180,
                &players,
                PLAYER_RADIUS,
            )
        }
        "belief" => {
            let empty = LocalField::default();
            let field = belief.as_ref().map_or(&empty, |belief| &belief.local_field);
            simulate_targeting_shot_with_local_field(
                &map,
                field,
                &shot,
                WorldParameters::default(),
                180,
                &players,
                PLAYER_RADIUS,
            )
        }
        "actual" => simulate_targeting_shot(
            &map,
            &shot,
            WorldParameters::default(),
            180,
            &players,
            PLAYER_RADIUS,
        ),
        _ => unreachable!(),
    }
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "model": model,
        "points": result.points,
        "termination": termination_name(result.termination),
        "velocity": velocity,
        "angle": angle,
    }))
}

fn compare_request(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let connection = open_database(path)?;
    let data = read_decision(&connection, capture_id, sequence)?;
    let events = read_events_before(&connection, capture_id, sequence)?;
    let actual = latest_actual_planets(&events)
        .ok_or("this capture has no disclosed planet map at this decision")?;
    let mut state = BotState::from_diagnostic_snapshot(&data["bot_state"])
        .map_err(|error| error.to_string())?;
    let actual_belief = MapBelief::new(PlanetMap { planets: actual }, None);
    state.set_map_belief(Some(actual_belief));
    let angle_step = data["angle_step_degrees"]
        .as_u64()
        .unwrap_or(10)
        .clamp(1, 180) as u16;
    let energy = data["energy"]
        .as_f64()
        .ok_or("captured energy is missing")?;
    let oracle_choice = TargetingPolicy::new(TargetingSettings {
        angle_step_degrees: angle_step,
        ..TargetingSettings::default()
    })
    .choose(&state, energy)
    .ok_or("current policy found no choice using the disclosed map")?;
    let oracle_choice = choice_json(&oracle_choice);
    let original = (!data["choice"].is_null()).then(|| data["choice"].clone());
    Ok(json!({
        "capture_id": capture_id,
        "sequence": sequence,
        "recorded_choice": original,
        "actual_map_choice_current_policy": oracle_choice,
        "note": "The actual-map choice reruns the current policy against the disclosed map and the recorded bot state. It is a counterfactual, not the choice recorded during this capture.",
    }))
}

fn physics_audit(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let sequence = required_id(parameters, "seq")?;
    let connection = open_database(path)?;
    let events = read_events_before(&connection, capture_id, sequence)?;
    let actual_planets = latest_actual_planets(&events)
        .ok_or("this capture has no disclosed planet map at this decision")?;
    let epoch_sequence = events
        .iter()
        .rev()
        .find(|(_, stored)| stored["event"]["type"] == "planets")
        .map(|(sequence, _)| *sequence)
        .ok_or("no disclosed map epoch before this decision")?;
    let map = PlanetMap {
        planets: actual_planets,
    };
    let world = WorldParameters::default();
    let mut players = BTreeMap::<u32, Point>::new();
    let mut pending = Vec::<(i64, u32, f64, f64, Option<Point>)>::new();
    let mut path_errors = Vec::<(f64, Value, Vec<Point>, Vec<Point>, usize)>::new();
    for (event_sequence, stored) in events {
        if event_sequence <= epoch_sequence {
            continue;
        }
        let recorded: RecordedEvent = serde_json::from_value(stored)
            .map_err(|error| format!("invalid recorded event: {error}"))?;
        let RecordedEvent::Inbound(event) = recorded else {
            continue;
        };
        match event {
            ServerEvent::PlayerPosition { player_id, x, y } => {
                players.insert(player_id, Point { x, y });
            }
            ServerEvent::PlayerLeave { player_id } => {
                players.remove(&player_id);
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => pending.push((
                event_sequence,
                player_id,
                angle,
                velocity,
                players.get(&player_id).cloned(),
            )),
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                points,
            } => {
                if points.is_empty() {
                    continue;
                }
                let Some(begin_index) = pending
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, id, begun_angle, begun_velocity, _))| {
                        *id == player_id && *begun_angle == angle && *begun_velocity == velocity
                    })
                    .min_by(|(_, left), (_, right)| {
                        let distance = |launch: &Option<Point>| {
                            launch.as_ref().map_or(f64::INFINITY, |launch| {
                                (f64::from(launch.x - points[0].x))
                                    .hypot(f64::from(launch.y - points[0].y))
                            })
                        };
                        distance(&left.4).total_cmp(&distance(&right.4))
                    })
                    .map(|(index, _)| index)
                else {
                    continue;
                };
                pending.remove(begin_index);
                if points.len() < 3 {
                    continue;
                }
                let observation = ShotObservation {
                    player_id,
                    angle_degrees: angle,
                    velocity,
                    points: points.clone(),
                };
                let prediction = simulate_shot(&map, &observation, world, points.len())
                    .map_err(|error| format!("simulation failed: {error}"))?;
                let timed_observed_len = points.len().saturating_sub(1);
                let timed_predicted_len =
                    prediction.points.len().saturating_sub(usize::from(matches!(
                        prediction.termination,
                        Termination::PlanetCollision(_) | Termination::BattlefieldMargin
                    )));
                let count = timed_observed_len.min(timed_predicted_len);
                if count == 0 {
                    continue;
                }
                let errors = points[..count]
                    .iter()
                    .zip(&prediction.points[..count])
                    .map(|(observed, predicted)| {
                        f64::from(observed.x - predicted.x)
                            .hypot(f64::from(observed.y - predicted.y))
                    })
                    .collect::<Vec<_>>();
                let rmse = (errors.iter().map(|error| error * error).sum::<f64>()
                    / (2 * errors.len()) as f64)
                    .sqrt();
                let max_error = errors.iter().copied().fold(0.0, f64::max);
                let detail = json!({
                    "shot_sequence": event_sequence,
                    "player_id": player_id,
                    "angle": angle,
                    "velocity": velocity,
                    "timed_samples": count,
                    "coordinate_rmse": rmse,
                    "max_position_error": max_error,
                    "rust_termination": termination_name(prediction.termination),
                });
                path_errors.push((rmse, detail, points, prediction.points, count));
            }
            _ => {}
        }
    }
    if path_errors.is_empty() {
        return Err("no completed shots with matching begin events in this map epoch".to_owned());
    }
    let mut all_errors = Vec::new();
    let mut weighted_squared_error = 0.0;
    let mut coordinate_count = 0usize;
    for (_, _, observed, predicted, count) in &path_errors {
        for (observed, predicted) in observed[..*count].iter().zip(&predicted[..*count]) {
            let error =
                f64::from(observed.x - predicted.x).hypot(f64::from(observed.y - predicted.y));
            all_errors.push(error);
            weighted_squared_error += error * error;
            coordinate_count += 2;
        }
    }
    all_errors.sort_by(f64::total_cmp);
    let p95_index = ((all_errors.len() as f64 * 0.95).ceil() as usize)
        .saturating_sub(1)
        .min(all_errors.len() - 1);
    let (worst_rmse, worst_detail, worst_observed, worst_predicted, _) = path_errors
        .iter()
        .max_by(|left, right| left.0.total_cmp(&right.0))
        .expect("nonempty path errors");
    Ok(json!({
        "capture_id": capture_id,
        "sequence": sequence,
        "epoch_sequence": epoch_sequence,
        "map_planets": map.planets.len(),
        "audited_shots": path_errors.len(),
        "timed_samples": all_errors.len(),
        "coordinate_rmse": (weighted_squared_error / coordinate_count as f64).sqrt(),
        "median_position_error": all_errors[all_errors.len()/2],
        "p95_position_error": all_errors[p95_index],
        "max_position_error": all_errors.last().copied(),
        "worst_shot": worst_detail,
        "worst_observed_points": worst_observed,
        "worst_rust_points": worst_predicted,
        "note": "Uses the disclosed planets for the current map epoch and compares timed samples only. Player collision paths are compared only up to the recorded terminal point; the server's internal launch position is transmitted as f32 in the path.",
        "worst_shot_rmse": worst_rmse,
    }))
}

fn choice_json(choice: &ShotChoice) -> Value {
    json!({
        "reason": format!("{:?}", choice.reason),
        "velocity": choice.velocity,
        "angle_degrees": choice.angle_degrees,
        "miss_distance": choice.miss_distance,
        "predicted_hit": choice.predicted_hit,
        "predicted_victim": choice.predicted_victim,
        "termination": if choice.predicted_hit {"player"} else {"not_player"},
    })
}

fn termination_name(termination: Termination) -> Value {
    match termination {
        Termination::PlanetCollision(index) => {
            json!({"kind":"planet_collision","planet_index":index})
        }
        Termination::PlayerCollision(id) => json!({"kind":"player_collision","player_id":id}),
        Termination::BattlefieldMargin => json!({"kind":"battlefield_margin"}),
        Termination::SampleLimit => json!({"kind":"sample_limit"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_map_lookup_uses_the_latest_planets_message_before_decision() {
        let events = vec![
            (
                1,
                json!({"event":{"type":"planets","planets":[{"x":1.0,"y":2.0,"radius":3.0,"mass":4.0}]}}),
            ),
            (
                9,
                json!({"event":{"type":"player_position","player_id":0,"x":1.0,"y":2.0}}),
            ),
            (
                10,
                json!({"event":{"type":"planets","planets":[{"x":5.0,"y":6.0,"radius":7.0,"mass":8.0}]}}),
            ),
        ];
        let planets = latest_actual_planets(&events).unwrap();
        assert_eq!(planets[0].x, 5.0);
        assert_eq!(planets[0].mass, 8.0);
    }

    #[test]
    fn planet_matching_is_one_to_one_closest_first_within_the_threshold() {
        let planet = |x: f64| Planet {
            x,
            y: 0.0,
            radius: 20.0,
            mass: 1000.0,
        };
        // Estimate 0 is nearer to actual 1 than actual 0; estimate 1 (far) must not steal it.
        let pairs = match_planets(
            &[planet(98.0), planet(10.0), planet(500.0)],
            &[planet(0.0), planet(100.0), planet(900.0)],
        );
        assert_eq!(
            pairs.iter().map(|p| (p.0, p.1)).collect::<Vec<_>>(),
            vec![(1, 0), (0, 1)]
        );
        let report = planet_match_json(
            &[planet(98.0), planet(10.0), planet(500.0)],
            &[planet(0.0), planet(100.0), planet(900.0)],
        );
        assert_eq!(report["missing_actual"], json!([2]));
        assert_eq!(report["unmatched_estimated"], json!([2]));
    }

    #[test]
    fn matching_is_greedy_closest_first_not_maximum_cardinality() {
        let planet = |x: f64| Planet {
            x,
            y: 0.0,
            radius: 20.0,
            mass: 1000.0,
        };
        // Estimate 0 takes actual 1 (distance 1); estimate 1 then has nothing within 80 units.
        let pairs = match_planets(&[planet(0.0), planet(79.0)], &[planet(1.0), planet(-79.0)]);
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].0, pairs[0].1), (0, 0));
    }

    #[test]
    fn decision_accuracy_needs_a_belief_and_an_earlier_disclosed_map() {
        let planet = |x: f64| Planet {
            x,
            y: 0.0,
            radius: 20.0,
            mass: 1000.0,
        };
        let disclosed = vec![(10, vec![planet(0.0), planet(500.0)])];
        let belief = json!({"planets": [planet(3.0)]});
        assert_eq!(decision_accuracy(&disclosed, 9, &belief), None);
        assert_eq!(decision_accuracy(&disclosed, 11, &Value::Null), None);
        let accuracy = decision_accuracy(&disclosed, 11, &belief).unwrap();
        assert_eq!(accuracy["missing_planets"], 1);
        assert_eq!(accuracy["matched_planets"], 1);
        assert_eq!(accuracy["median_center_error"], 3.0);
    }

    #[test]
    fn world_state_follows_events_between_decisions() {
        let event = |value: Value| (0, json!({"event": value}));
        let events = vec![
            event(json!({"type": "own_id", "player_id": 1})),
            event(json!({"type": "player_position", "player_id": 1, "x": 200.0, "y": 400.0})),
            event(json!({"type": "player_position", "player_id": 2, "x": 50.0, "y": 60.0})),
            event(json!({"type": "shot_begin", "player_id": 1, "angle": 0.0, "velocity": 8.0})),
            event(json!({"type": "shot_begin", "player_id": 2, "angle": 90.0, "velocity": 3.0})),
            event(
                json!({"type": "shot_fin", "player_id": 2, "angle": 90.0, "velocity": 3.0, "points": []}),
            ),
            event(json!({"type": "player_position", "player_id": 1, "x": 200.0, "y": 350.0})),
            event(json!({"type": "player_leave", "player_id": 2})),
        ];
        let world = world_state(&events, None);
        assert_eq!(world["own_id"], 1);
        assert_eq!(world["players"], json!({"1": {"x": 200.0, "y": 350.0}}));
        assert_eq!(world["in_flight"].as_array().unwrap().len(), 1);
        assert_eq!(world["in_flight"][0]["velocity"], 8.0);
        assert_eq!(world_state(&events, Some(2))["own_id"], 2);
        // Player 1 moved once, so its first position is remembered; player 2 left, so it has none.
        assert_eq!(world["ghosts"], json!({"1": [{"x": 200.0, "y": 400.0}]}));
        // Returning to an earlier spot shows no ghost under the player; a new map forgets them.
        let mut back = events.clone();
        back.push(event(
            json!({"type": "player_position", "player_id": 1, "x": 200.0, "y": 400.0}),
        ));
        assert_eq!(
            world_state(&back, None)["ghosts"],
            json!({"1": [{"x": 200.0, "y": 350.0}]})
        );
        back.push(event(json!({"type": "planets", "planets": []})));
        assert_eq!(world_state(&back, None)["ghosts"], json!({}));
    }

    #[test]
    fn median_averages_the_middle_pair() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[1.0, 5.0]), Some(3.0));
        assert_eq!(median(&[1.0, 2.0, 9.0]), Some(2.0));
    }

    #[test]
    fn page_loads_the_script_the_server_serves() {
        assert!(PAGE.contains(r#"<script src="/map_viewer.js"></script>"#));
        assert!(!SCRIPT.is_empty());
    }

    #[test]
    fn query_parser_reads_numeric_parameters() {
        let parameters = parse_query("id=42&seq=1171&velocity=8&angle=-12");
        assert_eq!(required_id(&parameters, "id").unwrap(), 42);
        assert_eq!(required_number(&parameters, "angle").unwrap(), -12.0);
    }

    #[test]
    fn selecting_a_timeline_shot_returns_its_recorded_endpoint() {
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-viewer-event-{}.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let metadata = einsteinbot::event_store::CaptureMetadata {
            started_at_unix_ms: 1,
            process_id: std::process::id(),
            host: "localhost".into(),
            port: 3490,
            client_name: "viewer-test".into(),
            protocol_version: 8,
        };
        let mut log = einsteinbot::event_store::EventLog::create(&path, &metadata).unwrap();
        log.append_server_event(
            2,
            &ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.0,
                velocity: 2.0,
                points: vec![Point { x: 10.0, y: 20.0 }],
            },
        )
        .unwrap();
        let parameters = BTreeMap::from([("id", "1"), ("seq", "event:0"), ("step", "25")]);
        let snapshot = selected_snapshot(&path, &parameters).unwrap();
        assert_eq!(snapshot["selection_kind"], "timeline_event");
        assert_eq!(snapshot["selected_shot"]["sequence"], 0);
        assert_eq!(snapshot["selected_shot"]["event"]["points"][0]["x"], 10.0);
        drop(log);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn trajectory_residual_samples_follow_recorded_paths_at_sample_resolution() {
        let map = PlanetMap { planets: vec![] };
        let points = vec![
            Point { x: 1.0, y: 2.0 },
            Point { x: 2.0, y: 4.0 },
            Point { x: 3.0, y: 6.0 },
            Point { x: 4.0, y: 8.0 },
            Point { x: 5.0, y: 10.0 },
        ];
        let samples = trajectory_residual_samples(&points, &map);
        assert_eq!(samples.len(), 2);
        assert!(
            samples
                .iter()
                .all(|sample| sample[2].hypot(sample[3]) == 0.0)
        );
    }

    #[test]
    fn trajectory_residual_samples_do_not_cross_planet_map_epochs() {
        let map = PlanetMap { planets: vec![] };
        let points = vec![
            Point { x: 1.0, y: 2.0 },
            Point { x: 2.0, y: 4.0 },
            Point { x: 3.0, y: 6.0 },
            Point { x: 4.0, y: 8.0 },
            Point { x: 5.0, y: 10.0 },
        ];
        assert!(trajectory_residual_samples_in_epoch(9, 10, &points, &map).is_empty());
        assert_eq!(
            trajectory_residual_samples_in_epoch(11, 10, &points, &map).len(),
            2
        );
    }

    #[test]
    fn field_grid_returns_compact_rows_for_the_selected_layer() {
        let data = json!({"map_belief":{"planets":[],"local_field":{"samples":[]}}});
        let actual = [];
        let grid = make_field_grid(
            &data,
            Some(&actual),
            1,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            "delta",
        )
        .unwrap();
        let sample = &grid["samples"][0];
        assert_eq!(sample.as_array().unwrap().len(), 15);
        assert_eq!(sample[2], 0.0);
        assert_eq!(sample[3], 0.0);
        assert_eq!(sample[13], false);
        // No validated field-only source: model disagreement is unavailable, not zero.
        assert!(sample[14].is_null());
    }

    #[test]
    fn field_grid_can_select_a_field_only_hypothesis_vector() {
        let data = json!({
            "map_belief": {
                "planets": [],
                "local_field": {"samples": []},
                "field_hypothesis": {
                    "x": 5.0,
                    "y": 5.0,
                    "mass": 100.0,
                    "support": [],
                    "baseline_continuation_rmse": 10.0,
                    "continuation_rmse": 1.0
                }
            }
        });
        let grid = make_field_grid(
            &data,
            None,
            1,
            Viewport {
                x: 0.0,
                y: 0.0,
                width: 1.0,
                height: 1.0,
            },
            "hypothesis_source",
        )
        .unwrap();
        let sample = &grid["samples"][0];
        assert!(sample[2].as_f64().is_some_and(|x| x > 0.0));
        assert!(sample[3].as_f64().is_some_and(|y| y > 0.0));
    }
}
