use serde::{Deserialize, Serialize};
use std::fmt;

const MAX_SHOT_POINTS: usize = 2_000;
const MAX_PLANETS: usize = 64;
const PLANET_RECORD_SIZE: u32 = 32;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Planet {
    pub x: f64,
    pub y: f64,
    pub radius: f64,
    pub mass: f64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerEvent {
    OwnId {
        player_id: u32,
    },
    PlayerLeave {
        player_id: u32,
    },
    PlayerPosition {
        player_id: u32,
        x: f32,
        y: f32,
    },
    ShotFinished {
        player_id: u32,
        points: Vec<Point>,
    },
    ShotBegin {
        player_id: u32,
        angle: f64,
        velocity: f64,
    },
    ShotFin {
        player_id: u32,
        angle: f64,
        velocity: f64,
        points: Vec<Point>,
    },
    GameMode {
        player_id: u32,
        mode: u32,
    },
    OwnEnergy {
        player_id: u32,
        energy: f64,
    },
    Planets {
        planets: Vec<Planet>,
    },
}

impl ServerEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::OwnId { .. } => "own_id",
            Self::PlayerLeave { .. } => "player_leave",
            Self::PlayerPosition { .. } => "player_position",
            Self::ShotFinished { .. } => "shot_finished",
            Self::ShotBegin { .. } => "shot_begin",
            Self::ShotFin { .. } => "shot_fin",
            Self::GameMode { .. } => "game_mode",
            Self::OwnEnergy { .. } => "own_energy",
            Self::Planets { .. } => "planets",
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientCommand {
    SetName { name: String },
    NegotiateProtocol { version: u32 },
    RequestEnergy,
    SetVelocity { velocity: f64 },
    SetAngle { angle: f64 },
    Disconnect,
}

impl ClientCommand {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::SetName { .. } => "set_name",
            Self::NegotiateProtocol { .. } => "negotiate_protocol",
            Self::RequestEnergy => "request_energy",
            Self::SetVelocity { .. } => "set_velocity",
            Self::SetAngle { .. } => "set_angle",
            Self::Disconnect => "disconnect",
        }
    }

    /// Encodes a semantic command as the newline-delimited text accepted by NewtonWars.
    pub fn wire_line(&self) -> Result<String, CommandError> {
        match self {
            Self::SetName { name } => {
                if name.contains(['\r', '\n']) {
                    return Err(CommandError("name cannot contain a line break"));
                }
                Ok(format!("n {name}\n"))
            }
            Self::NegotiateProtocol { version } => Ok(format!("b {version}\n")),
            Self::RequestEnergy => Ok("u\n".to_owned()),
            Self::SetVelocity { velocity } => finite_line("v", *velocity),
            Self::SetAngle { angle } => finite_line("", *angle),
            Self::Disconnect => Ok("q\n".to_owned()),
        }
    }
}

fn finite_line(command: &str, value: f64) -> Result<String, CommandError> {
    if !value.is_finite() {
        return Err(CommandError("numeric commands must be finite"));
    }
    if command.is_empty() {
        Ok(format!("{value}\n"))
    } else {
        Ok(format!("{command} {value}\n"))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandError(&'static str);

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for CommandError {}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "direction", content = "event", rename_all = "snake_case")]
pub enum RecordedEvent {
    Inbound(ServerEvent),
    Outbound(ClientCommand),
}

impl RecordedEvent {
    pub fn direction(&self) -> &'static str {
        match self {
            Self::Inbound(_) => "inbound",
            Self::Outbound(_) => "outbound",
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Inbound(event) => event.kind(),
            Self::Outbound(command) => command.kind(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtocolError(String);

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

/// Incremental decoder for the binary event stream after the text welcome prompt.
#[derive(Default)]
pub struct EventParser {
    buffer: Vec<u8>,
}

impl EventParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add bytes from a TCP read and return every complete event currently buffered.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<ServerEvent>, ProtocolError> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();

        loop {
            let Some((kind, frame_len)) = self.next_frame()? else {
                break;
            };
            let frame = self.buffer[..frame_len].to_vec();
            events.push(parse_frame(kind, &frame)?);
            self.buffer.drain(..frame_len);
        }

        Ok(events)
    }

    fn next_frame(&self) -> Result<Option<(u32, usize)>, ProtocolError> {
        if self.buffer.len() < 4 {
            return Ok(None);
        }

        let kind = read_u32(&self.buffer, 0);
        let fixed_len = match kind {
            1 | 2 => Some(8),
            3 => Some(16),
            5 => Some(24),
            7 => Some(12),
            8 => Some(16),
            4 => return self.variable_shot_finished_len(kind),
            6 => return self.variable_shot_fin_len(kind),
            9 => return self.variable_planets_len(kind),
            _ => return Err(ProtocolError(format!("unknown message type {kind}"))),
        };

        let frame_len = fixed_len.expect("all fixed message kinds have a length");
        Ok((self.buffer.len() >= frame_len).then_some((kind, frame_len)))
    }

    fn variable_shot_finished_len(&self, kind: u32) -> Result<Option<(u32, usize)>, ProtocolError> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let points = checked_count(read_u32(&self.buffer, 8), MAX_SHOT_POINTS, "shot point")?;
        let frame_len = 12 + points * 8;
        Ok((self.buffer.len() >= frame_len).then_some((kind, frame_len)))
    }

    fn variable_shot_fin_len(&self, kind: u32) -> Result<Option<(u32, usize)>, ProtocolError> {
        if self.buffer.len() < 28 {
            return Ok(None);
        }
        let points = checked_count(read_u32(&self.buffer, 24), MAX_SHOT_POINTS, "shot point")?;
        let frame_len = 28 + points * 8;
        Ok((self.buffer.len() >= frame_len).then_some((kind, frame_len)))
    }

    fn variable_planets_len(&self, kind: u32) -> Result<Option<(u32, usize)>, ProtocolError> {
        if self.buffer.len() < 12 {
            return Ok(None);
        }
        let count = checked_count(read_u32(&self.buffer, 4), MAX_PLANETS, "planet")?;
        let record_size = read_u32(&self.buffer, 8);
        if record_size != PLANET_RECORD_SIZE {
            return Err(ProtocolError(format!(
                "unexpected planet record size {record_size}; expected {PLANET_RECORD_SIZE}"
            )));
        }
        let frame_len = 12 + count * PLANET_RECORD_SIZE as usize;
        Ok((self.buffer.len() >= frame_len).then_some((kind, frame_len)))
    }
}

fn checked_count(value: u32, max: usize, label: &str) -> Result<usize, ProtocolError> {
    let value = value as usize;
    if value > max {
        return Err(ProtocolError(format!("too many {label}s: {value}")));
    }
    Ok(value)
}

fn parse_frame(kind: u32, frame: &[u8]) -> Result<ServerEvent, ProtocolError> {
    let event = match kind {
        1 => ServerEvent::OwnId {
            player_id: read_u32(frame, 4),
        },
        2 => ServerEvent::PlayerLeave {
            player_id: read_u32(frame, 4),
        },
        3 => ServerEvent::PlayerPosition {
            player_id: read_u32(frame, 4),
            x: read_f32(frame, 8),
            y: read_f32(frame, 12),
        },
        4 => {
            let count = read_u32(frame, 8) as usize;
            ServerEvent::ShotFinished {
                player_id: read_u32(frame, 4),
                points: read_points(frame, 12, count),
            }
        }
        5 => ServerEvent::ShotBegin {
            player_id: read_u32(frame, 4),
            angle: read_f64(frame, 8),
            velocity: read_f64(frame, 16),
        },
        6 => {
            let count = read_u32(frame, 24) as usize;
            ServerEvent::ShotFin {
                player_id: read_u32(frame, 4),
                angle: read_f64(frame, 8),
                velocity: read_f64(frame, 16),
                points: read_points(frame, 28, count),
            }
        }
        7 => ServerEvent::GameMode {
            player_id: read_u32(frame, 4),
            mode: read_u32(frame, 8),
        },
        8 => ServerEvent::OwnEnergy {
            player_id: read_u32(frame, 4),
            energy: read_f64(frame, 8),
        },
        9 => {
            let count = read_u32(frame, 4) as usize;
            let planets = (0..count)
                .map(|index| {
                    let offset = 12 + index * PLANET_RECORD_SIZE as usize;
                    Planet {
                        x: read_f64(frame, offset),
                        y: read_f64(frame, offset + 8),
                        radius: read_f64(frame, offset + 16),
                        mass: read_f64(frame, offset + 24),
                    }
                })
                .collect();
            ServerEvent::Planets { planets }
        }
        _ => return Err(ProtocolError(format!("unknown message type {kind}"))),
    };
    Ok(event)
}

fn read_points(frame: &[u8], offset: usize, count: usize) -> Vec<Point> {
    (0..count)
        .map(|index| {
            let point_offset = offset + index * 8;
            Point {
                x: read_f32(frame, point_offset),
                y: read_f32(frame, point_offset + 4),
            }
        })
        .collect()
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("frame length checked"),
    )
}

fn read_f32(bytes: &[u8], offset: usize) -> f32 {
    f32::from_ne_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("frame length checked"),
    )
}

fn read_f64(bytes: &[u8], offset: usize) -> f64 {
    f64::from_ne_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("frame length checked"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_bytes(value: u32) -> [u8; 4] {
        value.to_ne_bytes()
    }

    fn f32_bytes(value: f32) -> [u8; 4] {
        value.to_ne_bytes()
    }

    fn f64_bytes(value: f64) -> [u8; 8] {
        value.to_ne_bytes()
    }

    #[test]
    fn parses_fragmented_and_concatenated_events() {
        let mut bytes = Vec::new();
        bytes.extend(u32_bytes(1));
        bytes.extend(u32_bytes(7));
        bytes.extend(u32_bytes(3));
        bytes.extend(u32_bytes(2));
        bytes.extend(f32_bytes(12.5));
        bytes.extend(f32_bytes(-4.0));

        let mut parser = EventParser::new();
        assert!(parser.feed(&bytes[..5]).unwrap().is_empty());
        assert_eq!(
            parser.feed(&bytes[5..]).unwrap(),
            vec![
                ServerEvent::OwnId { player_id: 7 },
                ServerEvent::PlayerPosition {
                    player_id: 2,
                    x: 12.5,
                    y: -4.0,
                },
            ]
        );
    }

    #[test]
    fn parses_variable_shot_and_planet_records() {
        let mut bytes = Vec::new();
        bytes.extend(u32_bytes(6));
        bytes.extend(u32_bytes(4));
        bytes.extend(f64_bytes(1.25));
        bytes.extend(f64_bytes(9.5));
        bytes.extend(u32_bytes(1));
        bytes.extend(f32_bytes(2.0));
        bytes.extend(f32_bytes(3.0));
        bytes.extend(u32_bytes(9));
        bytes.extend(u32_bytes(1));
        bytes.extend(u32_bytes(32));
        bytes.extend(f64_bytes(10.0));
        bytes.extend(f64_bytes(20.0));
        bytes.extend(f64_bytes(2.0));
        bytes.extend(f64_bytes(100.0));

        let mut parser = EventParser::new();
        assert!(parser.feed(&bytes[..31]).unwrap().is_empty());
        let events = parser.feed(&bytes[31..]).unwrap();
        assert_eq!(
            events,
            vec![
                ServerEvent::ShotFin {
                    player_id: 4,
                    angle: 1.25,
                    velocity: 9.5,
                    points: vec![Point { x: 2.0, y: 3.0 }],
                },
                ServerEvent::Planets {
                    planets: vec![Planet {
                        x: 10.0,
                        y: 20.0,
                        radius: 2.0,
                        mass: 100.0,
                    }],
                },
            ]
        );
    }

    #[test]
    fn encodes_semantic_commands_and_rejects_line_breaks() {
        assert_eq!(
            ClientCommand::SetName {
                name: "Einstein".to_owned()
            }
            .wire_line()
            .unwrap(),
            "n Einstein\n"
        );
        assert!(
            ClientCommand::SetName {
                name: "bad\nq".to_owned()
            }
            .wire_line()
            .is_err()
        );
    }
}
