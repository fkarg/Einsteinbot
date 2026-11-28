//! Read-only local inspector for recorded targeting decisions.
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{Planet, Point, RecordedEvent, ServerEvent};
use einsteinbot::simulation::{
    ShotObservation, Termination, WorldParameters, simulate_shot, simulate_targeting_shot,
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
#[path = "map_viewer/fog.rs"]
mod fog;
#[path = "map_viewer/postmortem.rs"]
mod postmortem;
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
/// Drawn paths keep the points farther than this many world units from their simplified line.
const PATH_THINNING: f64 = 0.5;
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
        "/api/fog-timeline" => fog::fog_timeline_request(db_path, &parameters),
        "/api/fog-path" => fog::fog_path_request(db_path, &parameters),
        "/api/postmortem" => postmortem::postmortem_request(db_path, &parameters),
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
                    "search_state": search_state(belief),
                    "accuracy": accuracy,
                }))
            });
        captures.push(json!({
            "id": id,
            "started_at_unix_ms": started,
            "ended_at_unix_ms": ended,
            "kind": if metadata["host"] == "synthetic-scenario" { "scenario" } else { "live" },
            "name": metadata["client_name"],
            // Per-map live captures (2026-11-27 on) link their session; older ones lack it.
            "session": metadata["session"],
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
    // Each `Planets` message starts a new map epoch; carried-forward estimates must not cross one.
    let mut epoch_starts = Vec::new();
    let mut epoch_statement = connection
        .prepare("SELECT sequence FROM events WHERE capture_id=?1 AND event_type='planets' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    for sequence in epoch_statement
        .query_map([capture_id], |row| row.get::<_, i64>(0))
        .map_err(|error| error.to_string())?
    {
        epoch_starts.push(sequence.map_err(|error| error.to_string())?);
    }
    let feed = feed_events(&connection, capture_id)?;
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
                "map_updating": map_updating(&data),
                "map_planets": belief["planets"].as_array().map_or(0, Vec::len),
                "fit_rmse": belief["model_position_rmse"],
                "covariance": belief["covariance"].is_array(),
                "covariance_reason": belief["covariance_unavailable_reason"],
                "search_state": search_state(belief),
                "map_pending_events": data["map_pending_events"],
                "lag": map_lag(&data, &feed, sequence),
                "update": update_summary(&data["map_update_report"]),
                "map_epoch": epoch_starts
                    .partition_point(|start| *start <= sequence)
                    .checked_sub(1)
                    .map_or(0, |index| epoch_starts[index]),
            }))
        })
        .map_err(|error| error.to_string())?;
    let decisions = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(json!({"capture_id": capture_id, "decisions": decisions}))
}

/// Why the planet search stopped: `settled` when the belief records no limit, the recorded
/// `search_limit` otherwise, and null for captures that predate the field (unknown, not settled).
fn search_state(belief: &Value) -> Value {
    match belief.get("search_limit") {
        None => Value::Null,
        Some(Value::Null) => json!("settled"),
        Some(limit) => limit.clone(),
    }
}

/// The capture's events that the live client feeds to the estimator, as `(sequence, is_path)`,
/// with `None` for a `Planets` message (a map reset, which empties the queue).
fn feed_events(
    connection: &Connection,
    capture_id: i64,
) -> Result<Vec<(i64, Option<bool>)>, String> {
    let mut statement = connection
        .prepare("SELECT sequence, event_type FROM events WHERE capture_id=?1 AND event_type IN ('planets','shot_fin','player_position','player_leave') ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([capture_id], |row| {
            let kind: String = row.get(1)?;
            Ok((
                row.get::<_, i64>(0)?,
                (kind != "planets").then_some(kind == "shot_fin"),
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<_, _>>()
        .map_err(|error| error.to_string())
}

/// How many observed paths the decision's belief had not seen yet: `paths` (recorded as
/// `map_pending_paths`, or `inferred: true` when counted among the last `map_pending_events`
/// estimator inputs recorded before the decision in its map epoch; the multi-bot client can
/// record one input after a decision reads, so inferred counts may be one low). Null when the
/// capture records neither.
fn map_lag(data: &Value, feed: &[(i64, Option<bool>)], sequence: i64) -> Value {
    let events = data["map_pending_events"].as_u64();
    if let Some(paths) = data["map_pending_paths"].as_u64() {
        return json!({"paths": paths, "events": events, "inferred": false});
    }
    let Some(events) = events else {
        return Value::Null;
    };
    let before = &feed[..feed.partition_point(|(event, _)| *event < sequence)];
    let paths = before
        .iter()
        .rev()
        .map_while(|(_, is_path)| *is_path)
        .take(events as usize)
        .filter(|is_path| *is_path)
        .count();
    json!({"paths": paths, "events": events, "inferred": true})
}

/// The decision's `map_update_report` (work and timings of the estimator update that published
/// its belief) with each part marked `phase` (wall time on the coordinating thread) or not (leaf:
/// CPU time summed over threads, nested inside phases), plus `phase_ms`, the summed phase wall
/// time. Null when the decision recorded no report (older captures, oracle-map decisions).
fn update_summary(report: &Value) -> Value {
    let Some(parts) = report["parts"].as_array() else {
        return Value::Null;
    };
    let parts = parts
        .iter()
        .map(|part| {
            let mut part = part.clone();
            part["phase"] = json!(
                part["part"]
                    .as_str()
                    .is_some_and(|name| PHASE_PARTS.contains(&name))
            );
            part
        })
        .collect::<Vec<_>>();
    json!({
        "phase_ms": parts
            .iter()
            .filter(|part| part["phase"] == true)
            .filter_map(|part| part["total_ms"].as_f64())
            .sum::<f64>(),
        "evaluations": report["evaluations"],
        "violations": report["violations"],
        "parts": parts,
    })
}

/// `UpdateReport` phase parts (timed on the coordinating thread; their totals are wall time and
/// do not overlap). Leaf parts are CPU time summed over threads and nest inside the phases.
const PHASE_PARTS: [&str; 9] = [
    "noise",
    "warm_refit",
    "seeding",
    "birth_alone",
    "birth_joint",
    "deaths",
    "polish",
    "diagnostics",
    "residual_correction",
];

/// Whether observations were still queued for the estimator when the decision read its belief.
/// Scenario captures record `map_updating`; live captures record `map_pending_events`.
fn map_updating(data: &Value) -> Value {
    match (&data["map_updating"], data["map_pending_events"].as_u64()) {
        (Value::Bool(updating), _) => json!(updating),
        (_, Some(pending)) => json!(pending > 0),
        _ => Value::Null,
    }
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
    // `field` is accepted and ignored for older clients; every channel is always returned.
    let grid_step = parameters.get("step").map_or(Ok(FIELD_GRID_STEP), |step| {
        step.parse::<usize>()
            .map_err(|_| "invalid grid step".to_owned())
    })?;
    if !(1..=25).contains(&grid_step) {
        return Err("grid step must be 1 to 25 world units".to_owned());
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
    let field_grid = make_field_grid(&data, actual_planets.as_deref(), grid_step, view)?;
    // The fog of the belief shown, over the paths it had fitted (P26).
    let fog_grid = diagnostic_sequence
        .and_then(|belief_sequence| {
            let feed = feed_events(&connection, capture_id).ok()?;
            let lag = map_lag(&data, &feed, belief_sequence);
            fog::decision_fog(&events, belief_sequence, &data, &lag)
        })
        .map_or(Value::Null, |(fog, info)| fog::fog_grid(&fog, info));
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
    // Every path of the current map epoch, thinned for drawing, with its inferred outcome.
    let with_residuals = parameters.get("residuals") == Some(&"1");
    let belief_planets = trace_map
        .as_ref()
        .map_or(&[][..], |map| map.planets.as_slice());
    let outcomes = review::path_outcomes(&events, belief_planets, actual_planets.as_deref());
    let paths = events
        .iter()
        .filter(|(sequence, record)| {
            *sequence > map_epoch_start && record["event"]["type"] == "shot_fin"
        })
        .map(|(sequence, record)| {
            let event = &record["event"];
            let points =
                serde_json::from_value::<Vec<Point>>(event["points"].clone()).unwrap_or_default();
            let residual_samples = trace_map
                .as_ref()
                .filter(|_| with_residuals)
                .map(|map| trajectory_residual_samples(&points, map))
                .unwrap_or_default();
            let thinned = thin_path(&points, PATH_THINNING)
                .iter()
                .flat_map(|point| {
                    [point.x, point.y].map(|value| (f64::from(value) * 10.0).round() / 10.0)
                })
                .collect::<Vec<_>>();
            json!({
                "sequence": sequence,
                "player_id": event["player_id"],
                "velocity": event["velocity"],
                "angle": event["angle"],
                "samples": points.len(),
                "points": thinned,
                "outcome": outcomes.get(sequence).copied().unwrap_or("unknown"),
                "residual_samples": residual_samples,
                "label": roles.label(*sequence, event),
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
        "paths": paths,
        "actual_planets": actual_planets,
        "planet_match": planet_match,
        "world": world_state(&events, data["bot_state"]["own_id"].as_u64()),
        "field_grid": field_grid,
        "fog_grid": fog_grid,
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

/// A square, finite matrix from nested JSON arrays; anything else is unavailable.
fn matrix_from_json(value: &Value) -> Option<DMatrix<f64>> {
    let rows = value.as_array()?;
    let columns = rows.first()?.as_array()?.len();
    if columns == 0 || rows.len() != columns {
        return None;
    }
    let mut matrix = DMatrix::zeros(rows.len(), columns);
    for (row, cells) in rows.iter().enumerate() {
        let cells = cells.as_array().filter(|cells| cells.len() == columns)?;
        for (column, cell) in cells.iter().enumerate() {
            matrix[(row, column)] = cell.as_f64().filter(|value| value.is_finite())?;
        }
    }
    Some(matrix)
}

fn decode_belief(value: &Value) -> Result<Option<MapBelief>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let planets: Vec<Planet> = serde_json::from_value(value["planets"].clone())
        .map_err(|error| format!("invalid captured planets: {error}"))?;
    let map = PlanetMap { planets };
    let mut belief = MapBelief::new(map, None);
    // Captures from before the residual removal may carry local_field / field_hypothesis keys; ignored.
    belief.parameter_covariance = matrix_from_json(&value["covariance"]);
    belief.model_position_rmse = value["model_position_rmse"].as_f64();
    belief.contact_points = serde_json::from_value(
        value
            .get("contact_points")
            .cloned()
            .unwrap_or_else(|| json!([])),
    )
    .map_err(|error| error.to_string())?;
    // Older captures may carry fit diagnostics, approximate covariance and field evidence; the
    // estimator no longer records them and the viewer ignores them.
    Ok(Some(belief))
}

/// The field grid over `view` at `grid_step` spacing, row-major from (`origin_x`, `origin_y`),
/// as base64 little-endian f32 channels (`NaN` where a value is unavailable): `gravity_x/y` (the
/// belief's planets), `truth_x/y` (the disclosed map) and `sigma` (field σ from the fit
/// covariance). A channel is null when it has no value anywhere (no belief, no truth map, no
/// covariance).
fn make_field_grid(
    data: &Value,
    actual: Option<&[Planet]>,
    grid_step: usize,
    view: Viewport,
) -> Result<Value, String> {
    let belief = decode_belief(&data["map_belief"])?;
    let covariance = belief
        .as_ref()
        .and_then(|belief| belief.parameter_covariance.as_ref());
    let actual_map = actual.map(|planets| PlanetMap {
        planets: planets.to_vec(),
    });
    let world = WorldParameters::default();
    let start_x = (view.x as usize / grid_step) * grid_step;
    let start_y = (view.y as usize / grid_step) * grid_step;
    let end_x = (view.x + view.width).ceil() as usize;
    let end_y = (view.y + view.height).ceil() as usize;
    let mut channels: [Vec<f32>; 5] = Default::default();
    for y in (start_y..=end_y).step_by(grid_step) {
        for x in (start_x..=end_x).step_by(grid_step) {
            let position = Vec2::new(x as f64, y as f64);
            let gravity = belief
                .as_ref()
                .and_then(|belief| belief.map.acceleration(position).ok());
            let truth = actual_map
                .as_ref()
                .and_then(|map| map.acceleration(position).ok());
            let sigma = belief
                .as_ref()
                .zip(covariance)
                .and_then(|(belief, matrix)| {
                    belief
                        .map
                        .acceleration_covariance(position, matrix)
                        .ok()
                        .map(|covariance| (covariance.xx + covariance.yy).max(0.0).sqrt())
                });
            let values = [
                gravity.map(|v| v.x),
                gravity.map(|v| v.y),
                truth.map(|v| v.x),
                truth.map(|v| v.y),
                sigma,
            ];
            for (channel, value) in channels.iter_mut().zip(values) {
                channel.push(value.map_or(f32::NAN, |value| value as f32));
            }
        }
    }
    let [gravity_x, gravity_y, truth_x, truth_y, sigma] = channels.map(|channel| {
        if channel.iter().any(|value| value.is_finite()) {
            json!(base64_f32(&channel))
        } else {
            Value::Null
        }
    });
    Ok(json!({
        "gravity_x": gravity_x,
        "gravity_y": gravity_y,
        "truth_x": truth_x,
        "truth_y": truth_y,
        "sigma": sigma,
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

/// Standard base64 (with padding) of `values` as little-endian f32 bytes.
fn base64_f32(values: &[f32]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        for (index, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            out.push(if index <= chunk.len() {
                char::from(ALPHABET[(word >> shift) as usize & 63])
            } else {
                '='
            });
        }
    }
    out
}

/// The polyline with points farther than `tolerance` world units from the simplified line kept
/// (Ramer-Douglas-Peucker), always with the first and last point.
fn thin_path(points: &[Point], tolerance: f64) -> Vec<Point> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut stack = vec![(0, points.len() - 1)];
    let coordinates = |point: &Point| (f64::from(point.x), f64::from(point.y));
    while let Some((first, last)) = stack.pop() {
        let (ax, ay) = coordinates(&points[first]);
        let (bx, by) = coordinates(&points[last]);
        let (dx, dy) = (bx - ax, by - ay);
        let length = dx.hypot(dy);
        let mut farthest = (0.0, first);
        for (index, point) in points.iter().enumerate().take(last).skip(first + 1) {
            let (px, py) = coordinates(point);
            let distance = if length > 0.0 {
                ((px - ax) * dy - (py - ay) * dx).abs() / length
            } else {
                (px - ax).hypot(py - ay)
            };
            if distance > farthest.0 {
                farthest = (distance, index);
            }
        }
        if farthest.0 > tolerance {
            keep[farthest.1] = true;
            stack.push((first, farthest.1));
            stack.push((farthest.1, last));
        }
    }
    points
        .iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then(|| point.clone()))
        .collect()
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
        "belief" => belief
            .as_ref()
            .map(|belief| belief.map.clone())
            .unwrap_or(PlanetMap { planets: vec![] }),
        _ => return Err("model must be belief or actual".to_owned()),
    };
    // The recorded belief as the bot used it: planets plus its residual correction (P17).
    let correction = if model == "belief" {
        let events = read_events_before(&connection, capture_id, sequence)?;
        let feed = feed_events(&connection, capture_id)?;
        review::used_correction(&events, &feed, sequence, &data)
    } else {
        None
    };
    let shot = ShotObservation {
        player_id: own_id,
        angle_degrees: angle,
        velocity,
        points: vec![start],
    };
    let result = einsteinbot::simulation::simulate_corrected_targeting_shot(
        &map,
        correction.as_ref(),
        &shot,
        WorldParameters::default(),
        180,
        &players,
        PLAYER_RADIUS,
    )
    .map_err(|error| error.to_string())?;
    Ok(json!({
        "model": model,
        "residual_correction_applied": correction.is_some(),
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
    fn field_grid_returns_binary_channels_and_null_for_unavailable_ones() {
        let data =
            json!({"map_belief":{"planets":[{"x": 10.0, "y": 0.0, "radius": 2.0, "mass": 50.0}]}});
        let view = Viewport {
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        };
        let grid = make_field_grid(&data, None, 1, view).unwrap();
        assert_eq!(
            (grid["columns"].as_u64(), grid["rows"].as_u64()),
            (Some(2), Some(2))
        );
        // 4 f32 values = 16 bytes = 24 base64 characters.
        assert_eq!(grid["gravity_x"].as_str().unwrap().len(), 24);
        // No truth map and no covariance: unavailable, not zero.
        assert!(grid["truth_x"].is_null());
        assert!(grid["sigma"].is_null());
    }

    #[test]
    fn base64_f32_matches_the_standard_encoding() {
        // 1.0f32 is 00 00 80 3f little-endian.
        assert_eq!(base64_f32(&[1.0]), "AACAPw==");
        assert_eq!(base64_f32(&[1.0, 1.0]), "AACAPwAAgD8=");
    }

    #[test]
    fn path_thinning_keeps_ends_and_corners_and_drops_collinear_points() {
        let points = [(0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 3.0), (4.0, 6.0)]
            .map(|(x, y)| Point { x, y });
        let thinned = thin_path(&points, 0.5);
        assert_eq!(
            thinned.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>(),
            [(0.0, 0.0), (2.0, 0.0), (4.0, 6.0)]
        );
    }

    #[test]
    fn captures_with_removed_residual_fields_still_decode_and_ignore_them() {
        let data = json!({
            "map_belief": {
                "planets": [{"x": 10.0, "y": 0.0, "radius": 2.0, "mass": 50.0}],
                "local_field": {"samples": [{"x": 1.0}]},
                "field_hypothesis": {"x": 5.0, "y": 5.0, "mass": 100.0},
                "provisional_field_hypothesis": null,
                "hypothesis_local_field": {"samples": []}
            }
        });
        let belief = decode_belief(&data["map_belief"]).unwrap().unwrap();
        assert_eq!(belief.map.planets.len(), 1);
    }

    #[test]
    fn decisions_report_map_lag_and_the_update_report_of_their_belief() {
        use einsteinbot::estimator::{CeilingViolation, Part, PartReport, UpdateReport};
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-viewer-lag-{}.sqlite",
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
        let shot = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: vec![],
        };
        let position = ServerEvent::PlayerPosition {
            player_id: 1,
            x: 1.0,
            y: 1.0,
        };
        for event in [
            shot.clone(),
            ServerEvent::Planets { planets: vec![] },
            shot.clone(),
            shot.clone(),
            position,
            shot,
        ] {
            log.append(1, &RecordedEvent::Inbound(event)).unwrap();
        }
        let report = UpdateReport {
            evaluations: 612,
            parts: vec![
                PartReport {
                    part: Part::ResidualEval,
                    calls: 600,
                    total_ms: 900.0,
                    longest_ms: 2.0,
                },
                PartReport {
                    part: Part::WarmRefit,
                    calls: 1,
                    total_ms: 120.0,
                    longest_ms: 120.0,
                },
                PartReport {
                    part: Part::Polish,
                    calls: 1,
                    total_ms: 300.0,
                    longest_ms: 300.0,
                },
            ],
            violations: vec![CeilingViolation {
                part: Part::Polish,
                observed_ms: 300.0,
                ceiling_ms: 250.0,
            }],
        };
        for data in [
            // Old live capture: lag inferred from the recorded inputs since the map reset (three
            // paths and a position; the rest of the 5 are positions the client re-sends after a
            // reset, which are not recorded, and the path before the reset is not counted).
            json!({"map_belief": {"planets": []}, "map_pending_events": 5}),
            json!({"map_belief": {"planets": []}, "map_pending_events": 2, "map_pending_paths": 1,
                "map_update_report": serde_json::to_value(&report).unwrap()}),
            json!({"map_belief": {"planets": []}}),
        ] {
            log.append_diagnostic(1, "bot", "target_decision", &data)
                .unwrap();
        }
        let decisions = list_decisions(&path, 1).unwrap()["decisions"].clone();
        assert_eq!(
            decisions[0]["lag"],
            json!({"paths": 3, "events": 5, "inferred": true})
        );
        assert!(decisions[0]["update"].is_null());
        assert_eq!(
            decisions[1]["lag"],
            json!({"paths": 1, "events": 2, "inferred": false})
        );
        let update = &decisions[1]["update"];
        assert_eq!(update["evaluations"], 612);
        assert_eq!(update["phase_ms"], 420.0);
        assert_eq!(update["violations"][0]["part"], "polish");
        assert_eq!(update["violations"][0]["ceiling_ms"], 250.0);
        assert_eq!(update["parts"][0]["part"], "residual_eval");
        assert_eq!(update["parts"][0]["phase"], false);
        assert_eq!(update["parts"][1]["phase"], true);
        assert_eq!(update["parts"][1]["calls"], 1);
        assert_eq!(update["parts"][1]["longest_ms"], 120.0);
        // Neither pending count recorded (scenario captures before map_pending_events): unknown.
        assert!(decisions[2]["lag"].is_null());
        drop(log);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn decisions_report_search_state_and_covariance_for_current_and_old_beliefs() {
        use einsteinbot::estimator::{CovarianceUnavailableReason, SearchLimit};
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-viewer-decisions-{}.sqlite",
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
        let planet = Planet {
            x: 100.0,
            y: 100.0,
            radius: 20.0,
            mass: 800.0,
        };
        // Right after a path: no covariance yet, search work pending.
        let mut fresh = MapBelief::new(
            PlanetMap {
                planets: vec![planet.clone()],
            },
            None,
        );
        fresh.search_limit = Some(SearchLimit::WorkBudget);
        // After the slices and the exact refit: covariance, search settled.
        let polished = MapBelief::new(
            PlanetMap {
                planets: vec![planet],
            },
            Some(DMatrix::identity(3, 3)),
        );
        let mut empty = MapBelief::new(PlanetMap { planets: vec![] }, None);
        empty.covariance_unavailable_reason = Some(CovarianceUnavailableReason::NoPlanets);
        let live = |belief: &MapBelief, pending: u64| json!({"map_belief": belief.diagnostic_json(), "map_pending_events": pending});
        for data in [
            live(&fresh, 1),
            live(&polished, 0),
            live(&empty, 0),
            // A capture from before the v2 estimator: removed keys, no search_limit.
            json!({"map_belief": {"planets": [], "fit_converged": true,
                "approximate_covariance": [[1.0]], "field_evidence": {}}}),
        ] {
            log.append_diagnostic(1, "bot", "target_decision", &data)
                .unwrap();
        }
        let decisions = list_decisions(&path, 1).unwrap()["decisions"].clone();
        let field = |index: usize, key: &str| decisions[index][key].clone();
        assert_eq!(field(0, "search_state"), "work_budget");
        assert_eq!(field(0, "covariance"), false);
        assert_eq!(field(0, "covariance_reason"), "not_computed");
        assert_eq!(field(0, "map_updating"), true);
        assert_eq!(field(1, "search_state"), "settled");
        assert_eq!(field(1, "covariance"), true);
        assert!(field(1, "covariance_reason").is_null());
        assert_eq!(field(1, "map_updating"), false);
        assert_eq!(field(2, "covariance_reason"), "no_planets");
        // Old capture: search state and staleness are unknown (null), not settled or fresh.
        assert!(field(3, "search_state").is_null());
        assert!(field(3, "map_updating").is_null());
        assert_eq!(field(3, "covariance"), false);
        assert!(field(3, "covariance_reason").is_null());
        let removed = ["fit_converged", "approximate_covariance", "field_evidence"];
        assert!(removed.iter().all(|key| decisions[3].get(key).is_none()));
        drop(log);
        std::fs::remove_file(path).unwrap();
    }
}
