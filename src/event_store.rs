use crate::protocol::{RecordedEvent, ServerEvent};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::error::Error;
use std::path::Path;

pub type StoreResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// A per-map capture with fewer completed paths (`ShotFin`/`ShotFinished`, any player, debris
/// included) is deleted when it closes: nothing was learned there. Joining on the end card
/// gives zero (the server simulates nothing until the next map); any kill alone gives ten.
pub const MIN_COMPLETED_PATHS: usize = 3;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CaptureMetadata {
    pub started_at_unix_ms: i64,
    pub process_id: u32,
    pub host: String,
    pub port: u16,
    pub client_name: String,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ScenarioCaptureMetadata {
    pub name: String,
    pub family: String,
    pub defect_class: String,
    pub params: serde_json::Value,
    pub seed: Option<u64>,
    pub difficulty: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StoredEvent {
    pub sequence: i64,
    pub timestamp_unix_ms: i64,
    pub bot_name: Option<String>,
    pub event: RecordedEvent,
}

pub struct EventLog {
    connection: Connection,
    capture_id: i64,
    next_sequence: i64,
    /// Present for live recordings with one capture per map (`create_session`).
    session: Option<Session>,
}

/// How one closed per-map capture was handled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosedCapture {
    pub capture_id: i64,
    pub completed_paths: usize,
    /// False when it had fewer than `MIN_COMPLETED_PATHS` and was deleted.
    pub kept: bool,
}

/// Live-recording state: the metadata of the current map's capture and the latest world state
/// seen in recorded events, written at the start of the next capture so it replays on its own.
struct Session {
    base_metadata: serde_json::Value,
    metadata: serde_json::Value,
    map_index: u64,
    /// Whether the current capture began at an observed map boundary (not mid-map at connect).
    started_at_boundary: bool,
    started_at_unix_ms: i64,
    completed_paths: usize,
    game_mode: Option<ServerEvent>,
    own_ids: BTreeMap<Option<String>, u32>,
    energies: BTreeMap<String, f64>,
}

impl Session {
    fn observe(&mut self, bot_name: Option<&str>, event: &RecordedEvent) {
        let RecordedEvent::Inbound(event) = event else {
            return;
        };
        match event {
            ServerEvent::GameMode { .. } => self.game_mode = Some(event.clone()),
            ServerEvent::OwnId { player_id } => {
                self.own_ids.insert(bot_name.map(str::to_owned), *player_id);
            }
            ServerEvent::OwnEnergy { energy, .. } => {
                self.energies
                    .insert(bot_name.unwrap_or_default().to_owned(), *energy);
            }
            ServerEvent::ShotFin { .. } | ServerEvent::ShotFinished { .. } => {
                self.completed_paths += 1
            }
            _ => {}
        }
    }

    /// Initial events of a new capture: game mode, each bot's own id, the given positions.
    fn initial_events(&self, positions: &[(u32, f32, f32)]) -> Vec<(Option<String>, ServerEvent)> {
        let mut events: Vec<_> = self.game_mode.iter().map(|e| (None, e.clone())).collect();
        events.extend(
            self.own_ids
                .iter()
                .map(|(name, &player_id)| (name.clone(), ServerEvent::OwnId { player_id })),
        );
        events.extend(
            positions
                .iter()
                .map(|&(player_id, x, y)| (None, ServerEvent::PlayerPosition { player_id, x, y })),
        );
        events
    }
}

impl EventLog {
    /// Create one capture for one recording TCP connection in the SQLite database.
    pub fn create(path: impl AsRef<Path>, metadata: &CaptureMetadata) -> StoreResult<Self> {
        Self::create_inner(path, metadata, None)
    }

    /// Create a synthetic scenario capture with additive scenario labels in capture metadata.
    pub fn create_with_scenario_metadata(
        path: impl AsRef<Path>,
        metadata: &CaptureMetadata,
        scenario: &ScenarioCaptureMetadata,
    ) -> StoreResult<Self> {
        Self::create_inner(path, metadata, Some(scenario))
    }

    fn create_inner(
        path: impl AsRef<Path>,
        metadata: &CaptureMetadata,
        scenario: Option<&ScenarioCaptureMetadata>,
    ) -> StoreResult<Self> {
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS captures (
                 id INTEGER PRIMARY KEY,
                 started_at_unix_ms INTEGER NOT NULL,
                 ended_at_unix_ms INTEGER,
                 metadata_json TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS events (
                 capture_id INTEGER NOT NULL REFERENCES captures(id),
                 sequence INTEGER NOT NULL,
                 timestamp_unix_ms INTEGER NOT NULL,
                 bot_name TEXT,
                 direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound')),
                 event_type TEXT NOT NULL,
                 event_json TEXT NOT NULL,
                 PRIMARY KEY(capture_id, sequence)
             );
             CREATE TABLE IF NOT EXISTS diagnostics (
                 capture_id INTEGER NOT NULL REFERENCES captures(id),
                 sequence INTEGER NOT NULL,
                 timestamp_unix_ms INTEGER NOT NULL,
                 bot_name TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 data_json TEXT NOT NULL,
                 PRIMARY KEY(capture_id, sequence)
             );",
        )?;
        let has_bot_name = {
            let mut statement = connection.prepare("PRAGMA table_info(events)")?;
            let names = statement.query_map([], |row| row.get::<_, String>(1))?;
            names
                .collect::<Result<Vec<_>, _>>()?
                .iter()
                .any(|name| name == "bot_name")
        };
        if !has_bot_name {
            connection.execute("ALTER TABLE events ADD COLUMN bot_name TEXT", [])?;
        }

        let mut metadata_json = serde_json::to_value(metadata)?;
        metadata_json["build"] = serde_json::json!({
            "git_revision": option_env!("EINSTEINBOT_GIT_REVISION"),
            "source_changes": option_env!("EINSTEINBOT_GIT_DIRTY").and_then(|value| value.parse::<bool>().ok()),
        });
        if let Some(scenario) = scenario {
            metadata_json["scenario"] = serde_json::to_value(scenario)?;
        }
        let metadata_json = serde_json::to_string(&metadata_json)?;
        connection.execute(
            "INSERT INTO captures (started_at_unix_ms, metadata_json) VALUES (?1, ?2)",
            params![metadata.started_at_unix_ms, metadata_json],
        )?;
        let capture_id = connection.last_insert_rowid();

        Ok(Self {
            connection,
            capture_id,
            next_sequence: 0,
            session: None,
        })
    }

    /// Start a live recording with one capture per map. The first capture covers the map in
    /// progress at connect; `start_next_map` closes it at each map boundary and opens the next.
    /// All captures of one recording share `session.id` and number maps in `session.map_index`.
    pub fn create_session(path: impl AsRef<Path>, metadata: &CaptureMetadata) -> StoreResult<Self> {
        let mut log = Self::create_inner(path, metadata, None)?;
        let base_metadata: serde_json::Value = serde_json::from_str(&log.connection.query_row(
            "SELECT metadata_json FROM captures WHERE id = ?1",
            [log.capture_id],
            |row| row.get::<_, String>(0),
        )?)?;
        let mut session = Session {
            base_metadata,
            metadata: serde_json::Value::Null,
            map_index: 0,
            started_at_boundary: false,
            started_at_unix_ms: metadata.started_at_unix_ms,
            completed_paths: 0,
            game_mode: None,
            own_ids: BTreeMap::new(),
            energies: BTreeMap::new(),
        };
        session.metadata = map_metadata(&session, None, 0);
        log.write_metadata(&session.metadata)?;
        log.session = Some(session);
        Ok(log)
    }

    /// Close the current map's capture (deleting it when it has fewer than
    /// `MIN_COMPLETED_PATHS` paths) and open the next map's capture of the same session. The new
    /// capture starts with the known state as events (game mode, each bot's own id, and
    /// `positions`, the new map's player positions received so far; the latest energies go to
    /// metadata), so it replays on its own.
    pub fn start_next_map(
        &mut self,
        timestamp_unix_ms: i64,
        boundary_source: &str,
        positions: &[(u32, f32, f32)],
    ) -> StoreResult<ClosedCapture> {
        let closed = self.close_map(timestamp_unix_ms, true)?;
        let session = self
            .session
            .as_mut()
            .ok_or("not a per-map recording session")?;
        session.map_index += 1;
        session.started_at_boundary = true;
        session.started_at_unix_ms = timestamp_unix_ms;
        session.completed_paths = 0;
        let initial = session.initial_events(positions);
        session.metadata = map_metadata(session, Some(boundary_source), initial.len());
        self.connection.execute(
            "INSERT INTO captures (started_at_unix_ms, metadata_json) VALUES (?1, ?2)",
            params![timestamp_unix_ms, serde_json::to_string(&session.metadata)?],
        )?;
        self.capture_id = self.connection.last_insert_rowid();
        self.next_sequence = 0;
        if !closed.kept {
            self.delete_capture(closed.capture_id)?;
        }
        for (bot_name, event) in initial {
            self.append_for_bot(
                timestamp_unix_ms,
                bot_name.as_deref(),
                &RecordedEvent::Inbound(event),
            )?;
        }
        Ok(closed)
    }

    pub fn capture_id(&self) -> i64 {
        self.capture_id
    }

    /// Close the capture. A per-map session capture records how its map ended and is deleted
    /// when it has fewer than `MIN_COMPLETED_PATHS` completed paths.
    pub fn finish(&mut self, ended_at_unix_ms: i64) -> StoreResult<Option<ClosedCapture>> {
        if self.session.is_some() {
            let closed = self.close_map(ended_at_unix_ms, false)?;
            if !closed.kept {
                self.delete_capture(closed.capture_id)?;
            }
            return Ok(Some(closed));
        }
        self.connection.execute(
            "UPDATE captures SET ended_at_unix_ms = ?1 WHERE id = ?2",
            params![ended_at_unix_ms, self.capture_id],
        )?;
        Ok(None)
    }

    fn close_map(
        &mut self,
        ended_at_unix_ms: i64,
        at_boundary: bool,
    ) -> StoreResult<ClosedCapture> {
        let session = self
            .session
            .as_mut()
            .ok_or("not a per-map recording session")?;
        let closed = ClosedCapture {
            capture_id: self.capture_id,
            completed_paths: session.completed_paths,
            kept: session.completed_paths >= MIN_COMPLETED_PATHS,
        };
        if !closed.kept {
            // Deleted by the caller once the next capture exists, so that no id is reused.
            return Ok(closed);
        }
        // A map's duration is known only when both of its ends were observed boundaries;
        // a capture that joined mid-map or stopped before the map ended has none.
        let full = session.started_at_boundary && at_boundary;
        let map = &mut session.metadata["map"];
        map["ended_by"] = serde_json::json!(if at_boundary {
            "boundary"
        } else {
            "disconnect"
        });
        map["ended_at_unix_ms"] = serde_json::json!(ended_at_unix_ms);
        map["completed_paths"] = serde_json::json!(session.completed_paths);
        map["duration_ms"] = if full {
            serde_json::json!(ended_at_unix_ms - session.started_at_unix_ms)
        } else {
            serde_json::Value::Null
        };
        map["duration_unavailable_reason"] = match (session.started_at_boundary, at_boundary) {
            (true, true) => serde_json::Value::Null,
            (false, _) => serde_json::json!("joined_mid_map"),
            (true, false) => serde_json::json!("disconnected_before_map_end"),
        };
        let metadata = serde_json::to_string(&session.metadata)?;
        self.connection.execute(
            "UPDATE captures SET ended_at_unix_ms = ?1, metadata_json = ?2 WHERE id = ?3",
            params![ended_at_unix_ms, metadata, self.capture_id],
        )?;
        Ok(closed)
    }

    fn delete_capture(&self, capture_id: i64) -> StoreResult<()> {
        let transaction = self.connection.unchecked_transaction()?;
        for table in ["events", "diagnostics"] {
            transaction.execute(
                &format!("DELETE FROM {table} WHERE capture_id = ?1"),
                [capture_id],
            )?;
        }
        transaction.execute("DELETE FROM captures WHERE id = ?1", [capture_id])?;
        transaction.commit()?;
        Ok(())
    }

    fn write_metadata(&self, metadata: &serde_json::Value) -> StoreResult<()> {
        self.connection.execute(
            "UPDATE captures SET metadata_json = ?1 WHERE id = ?2",
            params![serde_json::to_string(metadata)?, self.capture_id],
        )?;
        Ok(())
    }

    pub fn append(&mut self, timestamp_unix_ms: i64, event: &RecordedEvent) -> StoreResult<i64> {
        self.append_for_bot(timestamp_unix_ms, None, event)
    }

    pub fn append_for_bot(
        &mut self,
        timestamp_unix_ms: i64,
        bot_name: Option<&str>,
        event: &RecordedEvent,
    ) -> StoreResult<i64> {
        if let Some(session) = &mut self.session {
            session.observe(bot_name, event);
        }
        let sequence = self.next_sequence;
        let event_json = serde_json::to_string(event)?;
        self.connection.execute(
            "INSERT INTO events
                (capture_id, sequence, timestamp_unix_ms, bot_name, direction, event_type, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.capture_id,
                sequence,
                timestamp_unix_ms,
                bot_name,
                event.direction(),
                event.kind(),
                event_json,
            ],
        )?;
        self.next_sequence += 1;
        Ok(sequence)
    }

    pub fn read_events(&self) -> StoreResult<Vec<StoredEvent>> {
        read_capture_events(&self.connection, self.capture_id)
    }

    /// Append an internal observation in the same sequence as protocol events.
    pub fn append_diagnostic(
        &mut self,
        timestamp_unix_ms: i64,
        bot_name: &str,
        kind: &str,
        data: &serde_json::Value,
    ) -> StoreResult<i64> {
        let sequence = self.next_sequence;
        self.connection.execute(
            "INSERT INTO diagnostics (capture_id, sequence, timestamp_unix_ms, bot_name, kind, data_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![self.capture_id, sequence, timestamp_unix_ms, bot_name, kind, serde_json::to_string(data)?],
        )?;
        self.next_sequence += 1;
        Ok(sequence)
    }

    /// Read the ordered events from an existing capture without opening a client session.
    pub fn read_capture(path: impl AsRef<Path>, capture_id: i64) -> StoreResult<Vec<StoredEvent>> {
        let connection = Connection::open(path)?;
        read_capture_events(&connection, capture_id)
    }

    pub fn append_server_event(
        &mut self,
        timestamp_unix_ms: i64,
        event: &ServerEvent,
    ) -> StoreResult<i64> {
        self.append(timestamp_unix_ms, &RecordedEvent::Inbound(event.clone()))
    }
}

/// The current map's capture metadata: the base (build, host, names, protocol) plus the session
/// link and how the map started.
fn map_metadata(
    session: &Session,
    boundary_source: Option<&str>,
    initial_events: usize,
) -> serde_json::Value {
    let mut metadata = session.base_metadata.clone();
    let base = &session.base_metadata;
    metadata["session"] = serde_json::json!({
        "id": format!("{}-{}", base["started_at_unix_ms"], base["process_id"]),
        "map_index": session.map_index,
    });
    metadata["map"] = serde_json::json!({
        "started_by": if session.started_at_boundary { "boundary" } else { "connect" },
        "boundary_source": boundary_source,
        "started_at_unix_ms": session.started_at_unix_ms,
    });
    if session.started_at_boundary {
        metadata["initial_state"] = serde_json::json!({
            "events": initial_events,
            "energies": session.energies,
        });
    }
    metadata
}

fn read_capture_events(connection: &Connection, capture_id: i64) -> StoreResult<Vec<StoredEvent>> {
    let mut statement = connection.prepare(
        "SELECT sequence, timestamp_unix_ms, bot_name, event_json
         FROM events WHERE capture_id = ?1 ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([capture_id], |row| {
        let sequence: i64 = row.get(0)?;
        let timestamp_unix_ms = row.get(1)?;
        let bot_name = row.get(2)?;
        let event_json: String = row.get(3)?;
        Ok((sequence, timestamp_unix_ms, bot_name, event_json))
    })?;

    rows.map(|row| {
        let (sequence, timestamp_unix_ms, bot_name, event_json) = row?;
        Ok(StoredEvent {
            sequence,
            timestamp_unix_ms,
            bot_name,
            event: serde_json::from_str(&event_json)?,
        })
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ClientCommand, Point};

    #[test]
    fn diagnostics_share_event_sequence_and_capture_build_metadata() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1,
            process_id: 42,
            host: "localhost".into(),
            port: 3490,
            client_name: "Curie".into(),
            protocol_version: 8,
        };
        let mut log = EventLog::create(":memory:", &metadata).unwrap();
        log.append_server_event(2, &ServerEvent::OwnId { player_id: 4 })
            .unwrap();
        assert_eq!(
            log.append_diagnostic(
                3,
                "Curie",
                "target_decision",
                &serde_json::json!({"energy": 9})
            )
            .unwrap(),
            1
        );
        assert_eq!(
            log.append_server_event(4, &ServerEvent::PlayerLeave { player_id: 5 })
                .unwrap(),
            2
        );
        let (seq, name, payload): (i64, String, String) = log
            .connection
            .query_row(
                "SELECT sequence, bot_name, data_json FROM diagnostics WHERE capture_id = ?1",
                [log.capture_id()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((seq, name), (1, "Curie".into()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap(),
            serde_json::json!({"energy": 9})
        );
        let metadata_json: String = log
            .connection
            .query_row(
                "SELECT metadata_json FROM captures WHERE id = ?1",
                [log.capture_id()],
                |row| row.get(0),
            )
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert!(
            metadata["build"]["git_revision"].is_string()
                || metadata["build"]["git_revision"].is_null()
        );
        assert!(metadata["build"].get("source_changes").is_some());
    }

    #[test]
    fn scenario_capture_metadata_is_additive_and_keeps_unknown_seed_fields_null() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1,
            process_id: 42,
            host: "synthetic-scenario".into(),
            port: 0,
            client_name: "two_planets_one_known".into(),
            protocol_version: 8,
        };
        let scenario = ScenarioCaptureMetadata {
            name: "two_planets_one_known".into(),
            family: "partial_map".into(),
            defect_class: "unseen_planet".into(),
            params: serde_json::json!({"truth_planet_count": 2}),
            seed: None,
            difficulty: None,
        };
        let log =
            EventLog::create_with_scenario_metadata(":memory:", &metadata, &scenario).unwrap();
        let text: String = log
            .connection
            .query_row(
                "SELECT metadata_json FROM captures WHERE id=?1",
                [log.capture_id()],
                |row| row.get(0),
            )
            .unwrap();
        let stored: serde_json::Value = serde_json::from_str(&text).unwrap();

        assert_eq!(stored["scenario"]["name"], "two_planets_one_known");
        assert_eq!(stored["scenario"]["family"], "partial_map");
        assert_eq!(stored["scenario"]["defect_class"], "unseen_planet");
        assert_eq!(stored["scenario"]["params"]["truth_planet_count"], 2);
        assert!(stored["scenario"]["seed"].is_null());
        assert!(stored["scenario"]["difficulty"].is_null());
        assert!(stored["build"].is_object());
        assert_eq!(stored["host"], "synthetic-scenario");
    }

    #[test]
    fn stores_semantic_events_in_sequence_order() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: "Einstein".to_owned(),
            protocol_version: 8,
        };
        let mut log = EventLog::create(":memory:", &metadata).unwrap();

        assert_eq!(
            log.append_server_event(1_001, &ServerEvent::OwnId { player_id: 3 })
                .unwrap(),
            0
        );
        assert_eq!(
            log.append(
                1_002,
                &RecordedEvent::Outbound(ClientCommand::SetAngle { angle: 45.0 })
            )
            .unwrap(),
            1
        );
        assert_eq!(
            log.append_server_event(
                1_003,
                &ServerEvent::ShotFinished {
                    player_id: 3,
                    points: vec![Point { x: 1.0, y: 2.0 }],
                },
            )
            .unwrap(),
            2
        );

        assert_eq!(
            log.read_events().unwrap(),
            vec![
                StoredEvent {
                    sequence: 0,
                    timestamp_unix_ms: 1_001,
                    bot_name: None,
                    event: RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 3 }),
                },
                StoredEvent {
                    sequence: 1,
                    timestamp_unix_ms: 1_002,
                    bot_name: None,
                    event: RecordedEvent::Outbound(ClientCommand::SetAngle { angle: 45.0 }),
                },
                StoredEvent {
                    sequence: 2,
                    timestamp_unix_ms: 1_003,
                    bot_name: None,
                    event: RecordedEvent::Inbound(ServerEvent::ShotFinished {
                        player_id: 3,
                        points: vec![Point { x: 1.0, y: 2.0 }],
                    }),
                },
            ]
        );
        assert!(log.capture_id() > 0);
    }

    #[test]
    fn captures_in_one_database_keep_events_separate() {
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-events-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let metadata = |client_name: &str| CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: client_name.to_owned(),
            protocol_version: 8,
        };
        let mut first = EventLog::create(&path, &metadata("Einstein")).unwrap();
        let mut second = EventLog::create(&path, &metadata("Curie")).unwrap();

        first
            .append_server_event(1_001, &ServerEvent::OwnId { player_id: 1 })
            .unwrap();
        second
            .append_server_event(1_002, &ServerEvent::OwnId { player_id: 2 })
            .unwrap();
        first.finish(2_000).unwrap();

        assert_ne!(first.capture_id(), second.capture_id());
        assert_eq!(
            first.read_events().unwrap()[0].event,
            RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 1 })
        );
        assert_eq!(
            second.read_events().unwrap()[0].event,
            RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 2 })
        );
        assert_eq!(
            EventLog::read_capture(&path, first.capture_id()).unwrap()[0].event,
            RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 1 })
        );

        let connection = Connection::open(&path).unwrap();
        let first_end_time: Option<i64> = connection
            .query_row(
                "SELECT ended_at_unix_ms FROM captures WHERE id = ?1",
                [first.capture_id()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_end_time, Some(2_000));

        drop(connection);
        drop(first);
        drop(second);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn tagged_events_keep_bot_identity_in_shared_capture() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: "local-manager".to_owned(),
            protocol_version: 8,
        };
        let mut log = EventLog::create(":memory:", &metadata).unwrap();
        log.append_for_bot(
            1_001,
            Some("Curie"),
            &RecordedEvent::Inbound(ServerEvent::OwnEnergy {
                player_id: 4,
                energy: 6.0,
            }),
        )
        .unwrap();
        log.append_for_bot(
            1_002,
            None,
            &RecordedEvent::Inbound(ServerEvent::PlayerLeave { player_id: 5 }),
        )
        .unwrap();
        let events = log.read_events().unwrap();
        assert_eq!(events[0].bot_name.as_deref(), Some("Curie"));
        assert_eq!(events[1].bot_name, None);
    }

    #[test]
    fn adds_bot_tag_column_to_an_existing_database() {
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-legacy-events-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let legacy = Connection::open(&path).unwrap();
        legacy
            .execute_batch(
                "CREATE TABLE captures (
                    id INTEGER PRIMARY KEY,
                    started_at_unix_ms INTEGER NOT NULL,
                    ended_at_unix_ms INTEGER,
                    metadata_json TEXT NOT NULL
                );
                CREATE TABLE events (
                    capture_id INTEGER NOT NULL REFERENCES captures(id),
                    sequence INTEGER NOT NULL,
                    timestamp_unix_ms INTEGER NOT NULL,
                    direction TEXT NOT NULL,
                    event_type TEXT NOT NULL,
                    event_json TEXT NOT NULL,
                    PRIMARY KEY(capture_id, sequence)
                );",
            )
            .unwrap();
        drop(legacy);

        let metadata = CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: "Curie".to_owned(),
            protocol_version: 8,
        };
        let mut log = EventLog::create(&path, &metadata).unwrap();
        log.append_for_bot(
            1_001,
            Some("Curie"),
            &RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 4 }),
        )
        .unwrap();
        assert_eq!(
            log.read_events().unwrap()[0].bot_name.as_deref(),
            Some("Curie")
        );
        drop(log);
        std::fs::remove_file(path).unwrap();
    }

    fn metadata_of(log: &EventLog, id: i64) -> Option<serde_json::Value> {
        log.connection
            .query_row(
                "SELECT metadata_json FROM captures WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .map(|text| serde_json::from_str(&text).unwrap())
    }

    fn shot(player_id: u32) -> RecordedEvent {
        RecordedEvent::Inbound(ServerEvent::ShotFin {
            player_id,
            angle: 1.0,
            velocity: 2.0,
            points: vec![Point { x: 1.0, y: 2.0 }],
        })
    }

    #[test]
    fn map_rollover_writes_self_contained_linked_captures() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: "Einstein,Curie".to_owned(),
            protocol_version: 8,
        };
        let mut log = EventLog::create_session(":memory:", &metadata).unwrap();
        let first = log.capture_id();
        let inbound = RecordedEvent::Inbound;
        log.append_for_bot(
            1_001,
            None,
            &inbound(ServerEvent::GameMode {
                player_id: 1,
                mode: 1,
            }),
        )
        .unwrap();
        log.append_for_bot(
            1_002,
            Some("Einstein"),
            &inbound(ServerEvent::OwnId { player_id: 1 }),
        )
        .unwrap();
        log.append_for_bot(
            1_003,
            Some("Curie"),
            &inbound(ServerEvent::OwnId { player_id: 2 }),
        )
        .unwrap();
        for (player_id, x) in [(1, 10.0), (2, 20.0)] {
            let event = ServerEvent::PlayerPosition {
                player_id,
                x,
                y: 5.0,
            };
            log.append_for_bot(1_004, None, &inbound(event)).unwrap();
        }
        let energy = ServerEvent::OwnEnergy {
            player_id: 2,
            energy: 7.5,
        };
        log.append_for_bot(1_006, Some("Curie"), &inbound(energy))
            .unwrap();
        for _ in 0..MIN_COMPLETED_PATHS {
            log.append_for_bot(1_007, None, &shot(1)).unwrap();
        }

        let closed = log
            .start_next_map(
                2_000,
                "all_players_repositioned",
                &[(1, 10.0, 5.0), (2, 20.0, 5.0)],
            )
            .unwrap();
        assert_eq!(
            closed,
            ClosedCapture {
                capture_id: first,
                completed_paths: MIN_COMPLETED_PATHS,
                kept: true
            }
        );
        let second = log.capture_id();
        assert_ne!(second, first);
        // The new capture replays on its own: game mode, each bot's id, the new map's positions.
        let events = log.read_events().unwrap();
        let initial: Vec<_> = events
            .iter()
            .map(|e| (e.sequence, e.bot_name.as_deref(), e.event.clone()))
            .collect();
        assert_eq!(
            initial,
            vec![
                (
                    0,
                    None,
                    inbound(ServerEvent::GameMode {
                        player_id: 1,
                        mode: 1
                    })
                ),
                (
                    1,
                    Some("Curie"),
                    inbound(ServerEvent::OwnId { player_id: 2 })
                ),
                (
                    2,
                    Some("Einstein"),
                    inbound(ServerEvent::OwnId { player_id: 1 })
                ),
                (
                    3,
                    None,
                    inbound(ServerEvent::PlayerPosition {
                        player_id: 1,
                        x: 10.0,
                        y: 5.0
                    })
                ),
                (
                    4,
                    None,
                    inbound(ServerEvent::PlayerPosition {
                        player_id: 2,
                        x: 20.0,
                        y: 5.0
                    })
                ),
            ]
        );
        let old = metadata_of(&log, first).unwrap();
        let new = metadata_of(&log, second).unwrap();
        assert_eq!(old["session"]["id"], new["session"]["id"]);
        assert_eq!(
            (
                old["session"]["map_index"].as_u64(),
                new["session"]["map_index"].as_u64()
            ),
            (Some(0), Some(1))
        );
        assert_eq!(old["map"]["started_by"], "connect");
        assert_eq!(old["map"]["ended_by"], "boundary");
        // Joined mid-map: no duration, never a guess.
        assert!(old["map"]["duration_ms"].is_null());
        assert_eq!(old["map"]["duration_unavailable_reason"], "joined_mid_map");
        assert_eq!(new["map"]["started_by"], "boundary");
        assert_eq!(new["map"]["boundary_source"], "all_players_repositioned");
        assert_eq!(new["initial_state"]["events"], 5);
        assert_eq!(new["initial_state"]["energies"]["Curie"], 7.5);
        assert_eq!(new["client_name"], "Einstein,Curie");
        assert!(new["build"].is_object());

        for _ in 0..MIN_COMPLETED_PATHS {
            log.append_for_bot(2_100, None, &shot(2)).unwrap();
        }
        log.start_next_map(5_000, "planets", &[]).unwrap();
        // A map observed from boundary to boundary has a duration.
        let full = metadata_of(&log, second).unwrap();
        assert_eq!(full["map"]["duration_ms"], 3_000);
        assert!(full["map"]["duration_unavailable_reason"].is_null());
        assert_eq!(full["map"]["completed_paths"], MIN_COMPLETED_PATHS);
    }

    #[test]
    fn captures_with_too_few_paths_are_deleted_when_they_close() {
        let metadata = CaptureMetadata {
            started_at_unix_ms: 1_000,
            process_id: 42,
            host: "127.0.0.1".to_owned(),
            port: 3490,
            client_name: "Einstein".to_owned(),
            protocol_version: 8,
        };
        let mut log = EventLog::create_session(":memory:", &metadata).unwrap();
        let end_card = log.capture_id();
        log.append_for_bot(
            1_001,
            Some("Einstein"),
            &RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 1 }),
        )
        .unwrap();
        log.append_diagnostic(1_002, "Einstein", "target_decision", &serde_json::json!({}))
            .unwrap();
        for _ in 1..MIN_COMPLETED_PATHS {
            log.append_for_bot(1_003, None, &shot(1)).unwrap();
        }
        let closed = log.start_next_map(2_000, "planets", &[]).unwrap();
        assert!(!closed.kept);
        assert!(metadata_of(&log, end_card).is_none());
        let rows: i64 = log
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM events WHERE capture_id = ?1) + (SELECT COUNT(*) FROM diagnostics WHERE capture_id = ?1)",
                [end_card],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        // The next capture still knows the bot's identity, and an empty last map goes too.
        assert_eq!(
            log.read_events().unwrap()[0].event,
            RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 1 })
        );
        let last = log.capture_id();
        assert_eq!(log.finish(3_000).unwrap().map(|c| c.kept), Some(false));
        assert!(metadata_of(&log, last).is_none());
    }
}
