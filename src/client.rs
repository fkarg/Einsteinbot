use crate::config::AppConfig;
use crate::dummy::commands_for_energy;
use crate::event_store::{CaptureMetadata, EventLog};
use crate::live_map::LiveMap;
use crate::map::PlanetMap;
use crate::protocol::{ClientCommand, EventParser, RecordedEvent, ServerEvent};
use crate::rate_limit::ByteRateLimiter;
use crate::strategy::{BotState, MapBelief, TargetingPolicy, TargetingSettings};
use std::error::Error;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
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

    if config.targeting && !config.dummy && !config.oracle_map_for_policy {
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
    let (map_feed, map_worker) =
        LiveMap::spawn(config.targeting && !config.dummy && !config.oracle_map_for_policy);
    let mut workers = Vec::new();
    for (index, name) in names.into_iter().enumerate() {
        let config = config.clone();
        let running = Arc::clone(&running);
        let event_log = event_log.clone();
        let map_feed = map_feed.clone();
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
    // Discard queued work and join the currently running fit before leaving the manager.
    map_feed.reset();
    drop(map_feed);
    if let Some(map_worker) = map_worker {
        if map_worker.join().is_err() && worker_error.is_none() {
            worker_error = Some("map fitter worker panicked".into());
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
    map_feed: &LiveMap,
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
    map_feed: &LiveMap,
) -> ClientResult<()> {
    let mut rate_limiter = ByteRateLimiter::new();
    stream.set_write_timeout(Some(SOCKET_POLL_INTERVAL))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let welcome = read_prompt(stream)?;
    print!("{}", String::from_utf8_lossy(&welcome));
    io::stdout().flush()?;

    send_command(
        stream,
        &event_log,
        name,
        &mut rate_limiter,
        running,
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
        running,
        ClientCommand::NegotiateProtocol {
            version: PROTOCOL_VERSION,
        },
    )?;

    stream.set_read_timeout(Some(SOCKET_POLL_INTERVAL))?;
    let mut parser = EventParser::new();
    let mut bot_state = BotState::default();
    let mut oracle_belief = None;
    let mut targeting_settings = TargetingSettings::default();
    targeting_settings.angle_step_degrees = [8, 10, 12][bot_index % 3];
    let target_policy = TargetingPolicy::new(targeting_settings);
    send_command(
        stream,
        &event_log,
        name,
        &mut rate_limiter,
        running,
        ClientCommand::RequestEnergy,
    )?;
    let mut last_energy_request = Instant::now();
    let mut buffer = [0u8; 4096];
    'session: loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                eprintln!("Server closed the connection");
                break;
            }
            Ok(length) => {
                for event in parser.feed(&buffer[..length])? {
                    if !running.load(Ordering::SeqCst) {
                        break 'session;
                    }
                    println!("{}", event_summary(&event));
                    bot_state.observe(&event);
                    if oracle_map_for_policy {
                        if let ServerEvent::Planets { planets } = &event {
                            oracle_belief = Some(MapBelief::new(
                                PlanetMap {
                                    planets: planets.clone(),
                                },
                                None,
                            ));
                            bot_state.set_map_belief(oracle_belief.clone());
                        }
                    }
                    if targeting && !dummy && !oracle_map_for_policy && bot_index == 0 {
                        match &event {
                            ServerEvent::Planets { .. } => {
                                map_feed.reset();
                                // Upstream can send our new position before Planets. Carry
                                // the tracked snapshot across the epoch's queue invalidation.
                                for (player_id, position) in bot_state.player_positions() {
                                    map_feed.observe(&ServerEvent::PlayerPosition {
                                        player_id: *player_id,
                                        x: position.x,
                                        y: position.y,
                                    })?;
                                }
                                eprintln!("LiveMap reset: new map epoch");
                            }
                            ServerEvent::ShotFin { .. }
                            | ServerEvent::PlayerPosition { .. }
                            | ServerEvent::PlayerLeave { .. } => map_feed.observe(&event)?,
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
                        let (map_updating, map_belief) =
                            if targeting && !dummy && !oracle_map_for_policy {
                                map_feed.snapshot()
                            } else {
                                (false, oracle_belief.clone())
                            };
                        let commands = if dummy {
                            commands_for_energy(energy, &mut rng)
                        } else if targeting {
                            if !oracle_map_for_policy {
                                bot_state.set_map_belief(map_belief.clone());
                            }
                            let started = Instant::now();
                            let choice = target_policy.choose(&bot_state, energy);
                            let search_duration_ms = started.elapsed().as_millis();
                            if let Some(log) = &event_log {
                                let belief = map_belief.as_ref().map(|belief| serde_json::json!({
                                    "planets": belief.map.planets,
                                    "contact_points": belief.contact_points,
                                    "covariance": belief.parameter_covariance.as_ref().map(|matrix| {
                                        (0..matrix.nrows()).map(|row| (0..matrix.ncols()).map(|col| matrix[(row, col)]).collect::<Vec<_>>()).collect::<Vec<_>>()
                                    }),
                                    "model_position_rmse": belief.model_position_rmse,
                                    "fit_converged": belief.fit_converged,
                                    "planet_contact_counts": belief.planet_contact_counts,
                                    "uncertain_associations": belief.uncertain_associations,
                                }));
                                let decision = choice.map(|choice| {
                                    serde_json::json!({
                                        "reason": format!("{:?}", choice.reason),
                                        "velocity": choice.velocity,
                                        "angle_degrees": choice.angle_degrees,
                                        "miss_distance": choice.miss_distance,
                                    "predicted_hit": choice.predicted_hit,
                                        "information_score": choice.information_score,
                                        "uncertainty_available": choice.uncertainty_available,
                                        "covered_fraction": choice.covered_fraction,
                                        "contact_before_target": choice.contact_before_target,
                                    })
                                });
                                log.lock().map_err(|_| "event log mutex poisoned")?.append_diagnostic(
                                    now_unix_ms()?, name, "target_decision", &serde_json::json!({
                                        "energy": energy,
                                        "map_updating": map_updating,
                                        "map_source": if oracle_map_for_policy { "oracle" } else { "online" },
                                        "map_belief": belief,
                                        "bot_state": bot_state.diagnostic_snapshot(),
                                        "angle_step_degrees": targeting_settings.angle_step_degrees,
                                        "search_duration_ms": search_duration_ms,
                                        "choice": decision,
                                    }),
                                )?;
                            }
                            choice
                                .map(|choice| {
                                    println!(
                                        "TargetChoice reason={:?} velocity={} angle={} miss={:.2} information={:.3} covariance={} covered={:.3} contact_before_target={} map_updating={}",
                                        choice.reason,
                                        choice.velocity,
                                        choice.angle_degrees,
                                        choice.miss_distance,
                                        choice.information_score,
                                    choice.uncertainty_available,
                                    choice.covered_fraction,
                                    choice.contact_before_target,
                                    map_updating
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
                                if !running.load(Ordering::SeqCst) {
                                    break 'session;
                                }
                                send_command(
                                    stream,
                                    &event_log,
                                    name,
                                    &mut rate_limiter,
                                    running,
                                    command,
                                )?;
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
                running,
                ClientCommand::RequestEnergy,
            )?;
            last_energy_request = Instant::now();
        }
    }

    // Closing TCP is sufficient to leave the game, and never waits for rate-limit credit.
    if !running.load(Ordering::SeqCst) {
        stream.shutdown(Shutdown::Both)?;
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
    running: &AtomicBool,
    command: ClientCommand,
) -> ClientResult<()> {
    let line = command.wire_line()?;
    if !rate_limiter.wait_for(line.len(), running)? {
        return Ok(());
    }
    append_event(
        event_log,
        bot_name.into(),
        now_unix_ms()?,
        &RecordedEvent::Outbound(command),
    )?;
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
    fn targeting_keeps_firing_while_the_map_worker_is_busy() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::mpsc;
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, fitter) = LiveMap::spawn_with(move |_, _, _| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            None
        });
        feed.observe(&ServerEvent::PlayerLeave { player_id: 99 })
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (shots_tx, shots_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket.write_all(b"welcome> ").unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            socket.write_all(b"> ").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let mut frames = Vec::new();
            frames.extend(1u32.to_ne_bytes());
            frames.extend(1u32.to_ne_bytes());
            for (id, x, y) in [(1u32, 100.0f32, 100.0f32), (2, 200.0, 100.0)] {
                frames.extend(3u32.to_ne_bytes());
                frames.extend(id.to_ne_bytes());
                frames.extend(x.to_ne_bytes());
                frames.extend(y.to_ne_bytes());
            }
            for _ in 0..2 {
                frames.extend(8u32.to_ne_bytes());
                frames.extend(1u32.to_ne_bytes());
                frames.extend(8f64.to_ne_bytes());
            }
            socket.write_all(&frames).unwrap();
            let mut shots = 0;
            let mut polls = 0;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.trim().parse::<f64>().is_ok() => {
                        shots += 1;
                        if shots == 2 {
                            break;
                        }
                    }
                    Ok(_) if line.trim() == "u" => {
                        polls += 1;
                        if polls == 3 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            shots_tx.send(shots).unwrap();
        });
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let worker_feed = feed.clone();
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-diagnostic-{}-{}.sqlite",
            std::process::id(),
            now_unix_ms().unwrap()
        ));
        let log = Arc::new(Mutex::new(
            EventLog::create(
                &path,
                &CaptureMetadata {
                    started_at_unix_ms: 1,
                    process_id: 42,
                    host: "localhost".into(),
                    port: 3490,
                    client_name: "Einstein".into(),
                    protocol_version: 8,
                },
            )
            .unwrap(),
        ));
        let worker_log = Arc::clone(&log);
        let client = std::thread::spawn(move || {
            let mut socket = TcpStream::connect(address).unwrap();
            run_session(
                &mut socket,
                "Einstein",
                1,
                false,
                true,
                false,
                &worker_running,
                Some(worker_log),
                &worker_feed,
            )
        });
        let shots = shots_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        running.store(false, Ordering::SeqCst);
        release_tx.send(()).unwrap();
        server.join().unwrap();
        let _ = client.join().unwrap();
        drop(feed);
        fitter.join().unwrap();
        assert_eq!(shots, 2, "fit work must not suppress energy-driven shots");
        let db = rusqlite::Connection::open(&path).unwrap();
        let data: String = db.query_row("SELECT data_json FROM diagnostics WHERE kind = 'target_decision' ORDER BY sequence LIMIT 1", [], |row| row.get(0)).unwrap();
        let decision: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(decision["energy"], 8.0);
        assert_eq!(decision["map_updating"], true);
        assert_eq!(decision["map_belief"], serde_json::Value::Null);
        assert_eq!(decision["bot_state"]["player_positions"]["2"]["x"], 200.0);
        assert!(decision["choice"]["velocity"].is_number());
        drop(db);
        drop(log);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn shutdown_interrupts_a_buffered_energy_command_backlog() {
        // Cancellation used to be checked only before socket reads: a buffered energy burst
        // kept firing and waiting for rate credit. Exhaust credit before cancelling to pin it.
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::mpsc;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            socket.write_all(b"welcome> ").unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            socket.write_all(b"> ").unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            line.clear();
            reader.read_line(&mut line).unwrap();
            let mut frames = Vec::new();
            for _ in 0..256 {
                frames.extend(8u32.to_ne_bytes());
                frames.extend(1u32.to_ne_bytes());
                frames.extend(8f64.to_ne_bytes());
            }
            socket.write_all(&frames).unwrap();
            let mut bytes = 0;
            while bytes < 512 {
                line.clear();
                let count = reader.read_line(&mut line).unwrap();
                assert!(count > 0);
                bytes += count;
            }
            ready_tx.send(()).unwrap();
            // Keep draining until cancellation closes the socket; do not force the client out.
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
            }
        });
        let (done_tx, done_rx) = mpsc::channel();
        let client = std::thread::spawn(move || {
            let (feed, fitter) = LiveMap::spawn(false);
            let mut socket = TcpStream::connect(address).unwrap();
            let result = run_session(
                &mut socket,
                "Einstein",
                0,
                true,
                false,
                false,
                &worker_running,
                None,
                &feed,
            );
            drop(fitter);
            done_tx
                .send(result.map_err(|error| error.to_string()))
                .unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        running.store(false, Ordering::SeqCst);
        assert!(
            done_rx
                .recv_timeout(Duration::from_millis(500))
                .unwrap()
                .is_ok()
        );
        client.join().unwrap();
        server.join().unwrap();
    }

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
