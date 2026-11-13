use crate::protocol::{RecordedEvent, ServerEvent};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::path::Path;

pub type StoreResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

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
pub struct StoredEvent {
    pub sequence: i64,
    pub timestamp_unix_ms: i64,
    pub event: RecordedEvent,
}

pub struct EventLog {
    connection: Connection,
    capture_id: i64,
    next_sequence: i64,
}

impl EventLog {
    /// Create one capture for one recording TCP connection in the SQLite database.
    pub fn create(path: impl AsRef<Path>, metadata: &CaptureMetadata) -> StoreResult<Self> {
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
                 direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound')),
                 event_type TEXT NOT NULL,
                 event_json TEXT NOT NULL,
                 PRIMARY KEY(capture_id, sequence)
             );",
        )?;

        let metadata_json = serde_json::to_string(metadata)?;
        connection.execute(
            "INSERT INTO captures (started_at_unix_ms, metadata_json) VALUES (?1, ?2)",
            params![metadata.started_at_unix_ms, metadata_json],
        )?;
        let capture_id = connection.last_insert_rowid();

        Ok(Self {
            connection,
            capture_id,
            next_sequence: 0,
        })
    }

    pub fn capture_id(&self) -> i64 {
        self.capture_id
    }

    pub fn finish(&mut self, ended_at_unix_ms: i64) -> StoreResult<()> {
        self.connection.execute(
            "UPDATE captures SET ended_at_unix_ms = ?1 WHERE id = ?2",
            params![ended_at_unix_ms, self.capture_id],
        )?;
        Ok(())
    }

    pub fn append(&mut self, timestamp_unix_ms: i64, event: &RecordedEvent) -> StoreResult<i64> {
        let sequence = self.next_sequence;
        let event_json = serde_json::to_string(event)?;
        self.connection.execute(
            "INSERT INTO events
                (capture_id, sequence, timestamp_unix_ms, direction, event_type, event_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                self.capture_id,
                sequence,
                timestamp_unix_ms,
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

fn read_capture_events(connection: &Connection, capture_id: i64) -> StoreResult<Vec<StoredEvent>> {
    let mut statement = connection.prepare(
        "SELECT sequence, timestamp_unix_ms, event_json
         FROM events WHERE capture_id = ?1 ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([capture_id], |row| {
        let sequence: i64 = row.get(0)?;
        let timestamp_unix_ms = row.get(1)?;
        let event_json: String = row.get(2)?;
        Ok((sequence, timestamp_unix_ms, event_json))
    })?;

    rows.map(|row| {
        let (sequence, timestamp_unix_ms, event_json) = row?;
        Ok(StoredEvent {
            sequence,
            timestamp_unix_ms,
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
                    event: RecordedEvent::Inbound(ServerEvent::OwnId { player_id: 3 }),
                },
                StoredEvent {
                    sequence: 1,
                    timestamp_unix_ms: 1_002,
                    event: RecordedEvent::Outbound(ClientCommand::SetAngle { angle: 45.0 }),
                },
                StoredEvent {
                    sequence: 2,
                    timestamp_unix_ms: 1_003,
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
}
