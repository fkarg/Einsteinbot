use crate::config::AppConfig;
use crate::dummy::commands_for_energy;
use crate::estimator::{LiveEstimator, LiveEstimatorSettings, ObservationOutcome};
use crate::event_store::{CaptureMetadata, EventLog};
use crate::map::PlanetMap;
use crate::protocol::{ClientCommand, EventParser, RecordedEvent, ServerEvent};
use crate::rate_limit::ByteRateLimiter;
use crate::simulation::WorldParameters;
use crate::strategy::{BotState, MapBelief, TargetingPolicy, TargetingSettings};
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: u32 = 8;
const ENERGY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);

type ClientResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
/// Map belief fitted once by the canonical worker and read by every targeting session.
type SharedMapBelief = Arc<Mutex<Option<MapBelief>>>;

pub fn run(config: &AppConfig) -> ClientResult<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
    ctrlc::set_handler(move || signal_running.store(false, Ordering::SeqCst))?;

    if config.targeting && !config.oracle_map_for_policy {
        eprintln!(
            "Targeting fits a live map from completed trajectories; early shots use a flat field until the first fit lands."
        );
    }
    let names = (0..config.clients)
        .map(|index| client_name(config, index))
        .collect::<Vec<_>>();
    eprintln!(
        "Connecting {} client(s) to {}:{} as {}",
        config.clients,
        config.host,
        config.port,
        names.join(", ")
    );
    let event_log = if config.record {
        Some(Arc::new(Mutex::new(EventLog::create(
            &config.db_path,
            &CaptureMetadata {
                started_at_unix_ms: now_unix_ms()?,
                process_id: std::process::id(),
                host: config.host.clone(),
                port: config.port,
                client_name: names.join(","),
                protocol_version: PROTOCOL_VERSION,
            },
        )?)))
    } else {
        None
    };
    let map_feed: SharedMapBelief = Arc::new(Mutex::new(None));
    let mut workers = Vec::new();
    for (index, name) in names.into_iter().enumerate() {
        let config = config.clone();
        let running = Arc::clone(&running);
        let event_log = event_log.clone();
        let map_feed = Arc::clone(&map_feed);
        workers.push(std::thread::spawn(move || {
            let result = run_worker(&config, index, &name, &running, event_log, &map_feed);
            if result.is_err() {
                running.store(false, Ordering::SeqCst);
            }
            result
        }));
    }
    let mut worker_error = None;
    for worker in workers {
        match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) if worker_error.is_none() => worker_error = Some(error),
            Err(_) if worker_error.is_none() => {
                worker_error = Some("client worker panicked".into())
            }
            _ => {}
        }
    }
    let finish_result = if let Some(log) = &event_log {
        log.lock()
            .map_err(|_| "event log mutex poisoned")?
            .finish(now_unix_ms()?)
    } else {
        Ok(())
    };
    if let Some(error) = worker_error {
        return Err(error);
    }
    finish_result?;
    Ok(())
}

fn client_name(config: &AppConfig, index: usize) -> String {
    if index == 0 {
        return config.name.clone();
    }
    let mut names = vec![config.name.clone()];
    for name in [
        "Einstein", "Curie", "Galilei", "Feynman", "Noether", "Faraday", "Maxwell", "Hawking",
        "Turing", "Herschel", "Hubble",
    ] {
        if !names.iter().any(|existing| existing == name) {
            names.push(name.to_owned());
        }
    }
    names
        .get(index)
        .cloned()
        .unwrap_or_else(|| format!("Scientist{index}"))
}

fn run_worker(
    config: &AppConfig,
    index: usize,
    name: &str,
    running: &AtomicBool,
    event_log: Option<Arc<Mutex<EventLog>>>,
    map_feed: &SharedMapBelief,
) -> ClientResult<()> {
    let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
    run_session(
        &mut stream,
        name,
        index,
        config.dummy,
        config.targeting,
        config.oracle_map_for_policy,
        running,
        event_log,
        map_feed,
    )
}

fn run_session(
    stream: &mut TcpStream,
    name: &str,
    bot_index: usize,
    dummy: bool,
    targeting: bool,
    oracle_map_for_policy: bool,
    running: &AtomicBool,
    event_log: Option<Arc<Mutex<EventLog>>>,
    map_feed: &SharedMapBelief,
) -> ClientResult<()> {
    let mut rate_limiter = ByteRateLimiter::new();
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let welcome = read_prompt(stream)?;
    print!("{}", String::from_utf8_lossy(&welcome));
    io::stdout().flush()?;

    send_command(
        stream,
        &event_log,
        name,
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
        &event_log,
        name,
        &mut rate_limiter,
        ClientCommand::NegotiateProtocol {
            version: PROTOCOL_VERSION,
        },
    )?;

    stream.set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;
    let mut parser = EventParser::new();
    let mut bot_state = BotState::default();
    let mut targeting_settings = TargetingSettings::default();
    targeting_settings.angle_step_degrees = [8, 10, 12][bot_index % 3];
    let target_policy = TargetingPolicy::new(targeting_settings);
    // The canonical connection owns the shared live estimator: it sees every world broadcast,
    // fits candidate maps from completed trajectories, and publishes beliefs for all sessions.
    let mut live_estimator = if targeting && !oracle_map_for_policy && bot_index == 0 {
        Some(LiveEstimator::new(
            WorldParameters::default(),
            LiveEstimatorSettings::default(),
        ))
    } else {
        None
    };
    send_command(
        stream,
        &event_log,
        name,
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
                    if let Some(estimator) = &mut live_estimator {
                        match &event {
                            ServerEvent::Planets { .. } => {
                                // Planet data identify a map epoch boundary, including the
                                // handshake event when the server runs with sendplanets 1.
                                estimator.reset();
                                *map_feed.lock().map_err(|_| "map belief mutex poisoned")? = None;
                                eprintln!("LiveMap reset: new map epoch");
                            }
                            ServerEvent::ShotFin { .. } => {
                                if estimator.observe(&event) == ObservationOutcome::Refitted {
                                    match estimator.best_map() {
                                        Some((map, covariance)) => {
                                            eprintln!(
                                                "LiveMapFit sources={} observations={}",
                                                map.planets.len(),
                                                estimator.observations().len()
                                            );
                                            *map_feed
                                                .lock()
                                                .map_err(|_| "map belief mutex poisoned")? =
                                                Some(MapBelief::new(map, covariance));
                                        }
                                        None => {
                                            eprintln!("LiveMapFit produced no candidate map");
                                            if let Some(error) = estimator.last_fit_error() {
                                                eprintln!("LiveMapFitError {error}");
                                            }
                                        }
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    if should_record_event(bot_index, &event) {
                        let bot_specific = is_bot_specific(&event);
                        append_event(
                            &event_log,
                            if bot_specific { Some(name) } else { None },
                            now_unix_ms()?,
                            &RecordedEvent::Inbound(event.clone()),
                        )?;
                    }
                    if let ServerEvent::OwnEnergy { energy, .. } = event {
                        let mut rng = rand::rng();
                        let commands = if dummy {
                            commands_for_energy(energy, &mut rng)
                        } else if targeting {
                            if !oracle_map_for_policy {
                                let belief = map_feed
                                    .lock()
                                    .map_err(|_| "map belief mutex poisoned")?
                                    .clone();
                                bot_state.set_map_belief(belief);
                            }
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
                                send_command(stream, &event_log, name, &mut rate_limiter, command)?;
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
                &event_log,
                name,
                &mut rate_limiter,
                ClientCommand::RequestEnergy,
            )?;
            last_energy_request = Instant::now();
        }
    }

    if shutdown_requested {
        send_command(
            stream,
            &event_log,
            name,
            &mut rate_limiter,
            ClientCommand::Disconnect,
        )?;
    }

    Ok(())
}

fn is_bot_specific(event: &ServerEvent) -> bool {
    matches!(
        event,
        ServerEvent::OwnId { .. } | ServerEvent::OwnEnergy { .. }
    )
}

fn should_record_event(bot_index: usize, event: &ServerEvent) -> bool {
    bot_index == 0 || is_bot_specific(event)
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
    event_log: &Option<Arc<Mutex<EventLog>>>,
    bot_name: &str,
    rate_limiter: &mut ByteRateLimiter,
    command: ClientCommand,
) -> ClientResult<()> {
    let line = command.wire_line()?;
    append_event(
        event_log,
        bot_name.into(),
        now_unix_ms()?,
        &RecordedEvent::Outbound(command),
    )?;
    rate_limiter.wait_for(line.len())?;
    stream.write_all(line.as_bytes())?;
    Ok(())
}

fn append_event(
    event_log: &Option<Arc<Mutex<EventLog>>>,
    bot_name: Option<&str>,
    timestamp_unix_ms: i64,
    event: &RecordedEvent,
) -> ClientResult<()> {
    if let Some(log) = event_log {
        log.lock()
            .map_err(|_| "event log mutex poisoned")?
            .append_for_bot(timestamp_unix_ms, bot_name, event)?;
    }
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

    #[test]
    fn only_canonical_bot_records_shared_world_events() {
        let world_event = ServerEvent::PlayerPosition {
            player_id: 9,
            x: 10.0,
            y: 20.0,
        };
        let own_event = ServerEvent::OwnEnergy {
            player_id: 3,
            energy: 4.0,
        };
        assert!(should_record_event(0, &world_event));
        assert!(!should_record_event(1, &world_event));
        assert!(should_record_event(1, &own_event));
    }

    #[test]
    fn local_client_names_are_unique_for_parameter_variants() {
        let config = AppConfig {
            host: "127.0.0.1".to_owned(),
            port: 3490,
            name: "Einstein".to_owned(),
            record: false,
            dummy: false,
            targeting: true,
            oracle_map_for_policy: false,
            clients: 3,
            db_path: "unused.sqlite".into(),
        };
        let names = (0..config.clients)
            .map(|index| client_name(&config, index))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 3);
        assert!(names.contains("Einstein"));
        assert!(names.contains("Curie"));
        assert!(names.contains("Galilei"));
    }
}

fn now_unix_ms() -> ClientResult<i64> {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    Ok(millis.try_into()?)
}
