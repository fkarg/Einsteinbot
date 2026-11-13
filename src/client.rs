use crate::config::AppConfig;
use crate::dummy::commands_for_energy;
use crate::event_store::{CaptureMetadata, EventLog};
use crate::map::PlanetMap;
use crate::protocol::{ClientCommand, EventParser, RecordedEvent, ServerEvent};
use crate::rate_limit::ByteRateLimiter;
use crate::strategy::{BotState, MapBelief, TargetingPolicy};
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: u32 = 8;
const ENERGY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);

type ClientResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

pub fn run(config: &AppConfig) -> ClientResult<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || signal_running.store(false, Ordering::SeqCst))?;

    eprintln!(
        "Connecting to {}:{} as {}",
        config.host, config.port, config.name
    );
    if config.targeting && !config.oracle_map_for_policy {
        eprintln!(
            "Targeting has no live estimated-map feed yet; using a flat-field simulator until a map belief is supplied."
        );
    }
    let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
    let mut event_log = if config.record {
        Some(EventLog::create(
            &config.db_path,
            &CaptureMetadata {
                started_at_unix_ms: now_unix_ms()?,
                process_id: std::process::id(),
                host: config.host.clone(),
                port: config.port,
                client_name: config.name.clone(),
                protocol_version: PROTOCOL_VERSION,
            },
        )?)
    } else {
        None
    };

    let result = run_session(
        &mut stream,
        &config.name,
        config.dummy,
        config.targeting,
        config.oracle_map_for_policy,
        &running,
        &mut event_log,
    );
    let finish_result = event_log
        .as_mut()
        .map(|log| log.finish(now_unix_ms()?))
        .transpose();

    result?;
    finish_result?;
    Ok(())
}

fn run_session(
    stream: &mut TcpStream,
    name: &str,
    dummy: bool,
    targeting: bool,
    oracle_map_for_policy: bool,
    running: &AtomicBool,
    event_log: &mut Option<EventLog>,
) -> ClientResult<()> {
    let mut rate_limiter = ByteRateLimiter::new();
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let welcome = read_prompt(stream)?;
    print!("{}", String::from_utf8_lossy(&welcome));
    io::stdout().flush()?;

    send_command(
        stream,
        event_log,
        &mut rate_limiter,
        ClientCommand::SetName {
            name: name.to_owned(),
        },
    )?;
    // While still in text mode the server emits a fresh prompt after each command.
    // Consume the prompt following the name before switching to binary bot events.
    let _ = read_prompt(stream)?;
    send_command(
        stream,
        event_log,
        &mut rate_limiter,
        ClientCommand::NegotiateProtocol {
            version: PROTOCOL_VERSION,
        },
    )?;

    stream.set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;
    let mut parser = EventParser::new();
    let mut bot_state = BotState::default();
    let target_policy = TargetingPolicy::default();
    send_command(
        stream,
        event_log,
        &mut rate_limiter,
        ClientCommand::RequestEnergy,
    )?;
    let mut last_energy_request = Instant::now();
    let mut buffer = [0u8; 4096];
    let mut shutdown_requested = false;

    loop {
        if !running.load(Ordering::SeqCst) {
            shutdown_requested = true;
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                eprintln!("Server closed the connection");
                break;
            }
            Ok(length) => {
                for event in parser.feed(&buffer[..length])? {
                    println!("{}", event_summary(&event));
                    bot_state.observe(&event);
                    if oracle_map_for_policy {
                        if let ServerEvent::Planets { planets } = &event {
                            bot_state.set_map_belief(Some(MapBelief::new(
                                PlanetMap {
                                    planets: planets.clone(),
                                },
                                None,
                            )));
                        }
                    }
                    if let Some(log) = event_log.as_mut() {
                        log.append(now_unix_ms()?, &RecordedEvent::Inbound(event.clone()))?;
                    }
                    if let ServerEvent::OwnEnergy { energy, .. } = event {
                        let mut rng = rand::rng();
                        let commands = if dummy {
                            commands_for_energy(energy, &mut rng)
                        } else if targeting {
                            target_policy
                                .choose(&bot_state, energy)
                                .map(|choice| {
                                    println!(
                                        "TargetChoice reason={:?} velocity={} angle={} miss={:.2} information={:.3}",
                                        choice.reason,
                                        choice.velocity,
                                        choice.angle_degrees,
                                        choice.miss_distance,
                                        choice.information_score
                                    );
                                    [
                                        ClientCommand::SetVelocity {
                                            velocity: f64::from(choice.velocity),
                                        },
                                        ClientCommand::SetAngle {
                                            angle: f64::from(choice.angle_degrees),
                                        },
                                    ]
                                })
                                .or_else(|| commands_for_energy(energy, &mut rng))
                        } else {
                            None
                        };
                        if let Some(commands) = commands {
                            for command in commands {
                                send_command(stream, event_log, &mut rate_limiter, command)?;
                            }
                        }
                    }
                }
            }
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock
                    || error.kind() == io::ErrorKind::TimedOut => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }

        if last_energy_request.elapsed() >= ENERGY_POLL_INTERVAL {
            send_command(
                stream,
                event_log,
                &mut rate_limiter,
                ClientCommand::RequestEnergy,
            )?;
            last_energy_request = Instant::now();
        }
    }

    if shutdown_requested {
        send_command(
            stream,
            event_log,
            &mut rate_limiter,
            ClientCommand::Disconnect,
        )?;
    }

    Ok(())
}

fn event_summary(event: &ServerEvent) -> String {
    match event {
        ServerEvent::ShotFinished { player_id, points } => {
            format!("ShotFinished player={player_id} points={}", points.len())
        }
        ServerEvent::ShotFin {
            player_id,
            angle,
            velocity,
            points,
        } => format!(
            "ShotFin player={player_id} angle={angle:.1} velocity={velocity:.1} points={}",
            points.len()
        ),
        ServerEvent::Planets { planets } => format!("Planets count={}", planets.len()),
        event => format!("{event:?}"),
    }
}

fn send_command(
    stream: &mut TcpStream,
    event_log: &mut Option<EventLog>,
    rate_limiter: &mut ByteRateLimiter,
    command: ClientCommand,
) -> ClientResult<()> {
    let line = command.wire_line()?;
    if let Some(log) = event_log.as_mut() {
        log.append(now_unix_ms()?, &RecordedEvent::Outbound(command))?;
    }
    rate_limiter.wait_for(line.len())?;
    stream.write_all(line.as_bytes())?;
    Ok(())
}

fn read_prompt(reader: &mut impl Read) -> ClientResult<Vec<u8>> {
    let mut welcome = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "server closed before sending its welcome prompt",
                )
                .into());
            }
            Ok(_) => {
                welcome.push(byte[0]);
                if welcome.ends_with(b"> ") {
                    return Ok(welcome);
                }
                if welcome.len() > 4096 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "server welcome exceeded 4096 bytes without a prompt",
                    )
                    .into());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn reads_text_prompts_separately_before_binary_mode() {
        let mut stream = Cursor::new(b"welcome> > ".to_vec());

        assert_eq!(read_prompt(&mut stream).unwrap(), b"welcome> ");
        assert_eq!(read_prompt(&mut stream).unwrap(), b"> ");
    }
}

fn now_unix_ms() -> ClientResult<i64> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(millis.try_into()?)
}
