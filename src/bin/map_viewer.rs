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
use rusqlite::{Connection, OpenFlags, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

const PAGE: &str = include_str!("../../web/map_viewer.html");
const DEFAULT_PORT: u16 = 8765;
const PLAYER_RADIUS: f64 = 4.0;
const FIELD_GRID_STEP: usize = 25;

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
                if let Err(error) = handle(stream, &db_path) {
                    eprintln!("map_viewer request failed: {error}");
                }
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
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let parameters = parse_query(query);
    let result = match path {
        "/api/captures" => list_captures(db_path),
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

fn list_decisions(path: &PathBuf, capture_id: i64) -> Result<Value, String> {
    let connection = open_database(path)?;
    let mut statement = connection
        .prepare("SELECT sequence, timestamp_unix_ms, bot_name, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([capture_id], |row| {
            let data: String = row.get(3)?;
            let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
            let belief = &data["map_belief"];
            Ok(json!({
                "sequence": row.get::<_, i64>(0)?,
                "timestamp_unix_ms": row.get::<_, i64>(1)?,
                "bot_name": row.get::<_, String>(2)?,
                "energy": data["energy"],
                "choice": data["choice"],
                "map_updating": data["map_updating"],
                "map_planets": belief["planets"].as_array().map_or(0, Vec::len),
                "fit_rmse": belief["model_position_rmse"],
                "covariance": belief["covariance"].is_array(),
            }))
        })
        .map_err(|error| error.to_string())?;
    let decisions = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(json!({"capture_id": capture_id, "decisions": decisions}))
}

fn capture_timeline(path: &PathBuf, parameters: &BTreeMap<&str, &str>) -> Result<Value, String> {
    let capture_id = required_id(parameters, "id")?;
    let connection = open_database(path)?;
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
        Ok(json!({"sequence":sequence,"timestamp_unix_ms":row.get::<_,i64>(1)?,"kind":event_type,"detail":detail}))
    }).map_err(|error| error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
    let mut statement = connection.prepare("SELECT sequence, timestamp_unix_ms, bot_name, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .map_err(|error| error.to_string())?;
    let decisions = statement.query_map([capture_id], |row| {
        let sequence: i64 = row.get(0)?; let timestamp: i64 = row.get(1)?; let bot: String = row.get(2)?;
        let data: String = row.get(3)?; let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let choice = &data["choice"];
        Ok(json!({"sequence":sequence,"timestamp_unix_ms":timestamp,"kind":"decision","detail":format!("{bot} chose v{} at {}° ({})", choice["velocity"], choice["angle_degrees"], choice["reason"].as_str().unwrap_or("no shot"))}))
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
        .prepare("SELECT sequence, event_json FROM events WHERE capture_id=?1 AND sequence<?2 AND event_type IN ('player_position','player_leave','shot_begin','shot_fin','planets') ORDER BY sequence")
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

fn latest_actual_planets(events: &[(i64, Value)]) -> Option<Vec<Planet>> {
    events.iter().rev().find_map(|(_, stored)| {
        let event = &stored["event"];
        (event["type"] == "planets")
            .then(|| serde_json::from_value(event["planets"].clone()).ok())
            .flatten()
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
    let field_grid = make_field_grid(&data, actual_planets.as_deref())?;
    let trajectories = events
        .iter()
        .filter(|(_, record)| record["event"]["type"] == "shot_fin")
        .rev()
        .take(60)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|(sequence, record)| json!({"sequence": sequence, "record": record}))
        .collect::<Vec<_>>();
    Ok(json!({
        "capture_id": capture_id,
        "sequence": sequence,
        "diagnostic": data,
        "diagnostic_sequence": diagnostic_sequence,
        "selection_kind": if is_decision {"decision"} else if selection.starts_with("event:") {"timeline_event"} else {"latest_capture_state"},
        "events": trajectories,
        "actual_planets": actual_planets,
        "field_grid": field_grid,
    }))
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

fn make_field_grid(data: &Value, actual: Option<&[Planet]>) -> Result<Value, String> {
    let belief = decode_belief(&data["map_belief"])?;
    let covariance = belief
        .as_ref()
        .and_then(|belief| belief.parameter_covariance.as_ref());
    let actual_map = actual.map(|planets| PlanetMap {
        planets: planets.to_vec(),
    });
    let world = WorldParameters::default();
    let mut samples = Vec::new();
    for y in (0..=world.battlefield_height as usize).step_by(FIELD_GRID_STEP) {
        for x in (0..=world.battlefield_width as usize).step_by(FIELD_GRID_STEP) {
            let position = Vec2::new(x as f64, y as f64);
            let Some(belief) = &belief else {
                let oracle = actual_map
                    .as_ref()
                    .and_then(|map| map.acceleration(position).ok());
                samples.push(json!({"x":x,"y":y,"gravity":null,"total":null,"oracle":oracle.map(|v|json!({"x":v.x,"y":v.y})),"sigma":null,"supported":false,"hypothesis_proxy":null,"actual_gravity":oracle.map(|v| v.x.hypot(v.y))}));
                continue;
            };
            let physical = belief.map.acceleration(position).ok();
            let residual_observed = belief.local_field.residual(position);
            let hypothesis = belief.field_hypothesis.as_ref().and_then(|hypothesis| {
                hypothesis.acceleration(position).map(|gravity| {
                    let local = belief
                        .hypothesis_local_field
                        .as_ref()
                        .and_then(|field| field.residual(position))
                        .unwrap_or_default();
                    (gravity.x + local.x, gravity.y + local.y)
                })
            });
            let has_residual = residual_observed.is_some();
            let residual = residual_observed.unwrap_or_default();
            let gravity =
                physical.map(|physical| (physical.x + residual.x, physical.y + residual.y));
            let hypothesis_local = belief
                .hypothesis_local_field
                .as_ref()
                .and_then(|field| field.residual(position))
                .unwrap_or_default();
            let hypothesis_source = belief
                .field_hypothesis
                .as_ref()
                .and_then(|hypothesis| hypothesis.acceleration(position));
            let hypothesis_extra = hypothesis_source.map(|source| {
                Vec2::new(source.x + hypothesis_local.x, source.y + hypothesis_local.y)
            });
            let hypothesis_gravity = physical
                .zip(hypothesis)
                .map(|(physical, extra)| (physical.x + extra.0, physical.y + extra.1));
            let actual_vector = actual_map
                .as_ref()
                .and_then(|map| map.acceleration(position).ok());
            let delta = gravity
                .zip(actual_vector)
                .map(|(estimate, oracle)| Vec2::new(estimate.0 - oracle.x, estimate.1 - oracle.y));
            let hypothesis_delta = hypothesis_gravity
                .zip(actual_vector)
                .map(|(estimate, oracle)| Vec2::new(estimate.0 - oracle.x, estimate.1 - oracle.y));
            let sigma = covariance
                .and_then(|matrix| belief.map.acceleration_covariance(position, matrix).ok())
                .map(|covariance| (covariance.xx + covariance.yy).max(0.0).sqrt());
            let hypothesis_proxy = belief
                .field_hypothesis
                .as_ref()
                .map(|hypothesis| hypothesis.relative_uncertainty_proxy(position));
            samples.push(json!({
                "x":x,"y":y,
                "physical":physical.map(|v|json!({"x":v.x,"y":v.y})),
                "residual":json!({"x":residual.x,"y":residual.y}),
                "total":gravity.map(|(x,y)|json!({"x":x,"y":y})),
                "hypothesis_source":hypothesis_source.map(|v|json!({"x":v.x,"y":v.y})),
                "hypothesis_residual":json!({"x":hypothesis_local.x,"y":hypothesis_local.y}),
                "hypothesis_extra":hypothesis_extra.map(|v|json!({"x":v.x,"y":v.y})),
                "hypothesis_total":hypothesis_gravity.map(|(x,y)|json!({"x":x,"y":y})),
                "oracle":actual_vector.map(|v|json!({"x":v.x,"y":v.y})),
                "delta":delta.map(|v|json!({"x":v.x,"y":v.y})),
                "hypothesis_delta":hypothesis_delta.map(|v|json!({"x":v.x,"y":v.y})),
                "gravity":gravity.map(|(ax,ay)| ax.hypot(ay)),
                "hypothesis_gravity":hypothesis_gravity.map(|(ax,ay)| ax.hypot(ay)),
                "sigma":sigma,
                "supported":has_residual,
                "hypothesis_proxy":hypothesis_proxy,
                "actual_gravity":actual_map.as_ref().and_then(|map| map.acceleration(position).ok()).map(|v| v.x.hypot(v.y)),
            }));
        }
    }
    Ok(json!({
        "samples": samples,
        "width": world.battlefield_width,
        "height": world.battlefield_height,
        "step": FIELD_GRID_STEP,
        "has_covariance": covariance.is_some(),
        "has_belief": belief.is_some(),
    }))
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
    fn query_parser_reads_numeric_parameters() {
        let parameters = parse_query("id=42&seq=1171&velocity=8&angle=-12");
        assert_eq!(required_id(&parameters, "id").unwrap(), 42);
        assert_eq!(required_number(&parameters, "angle").unwrap(), -12.0);
    }
}
