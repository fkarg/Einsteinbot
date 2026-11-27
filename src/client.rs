use crate::config::AppConfig;
use crate::dummy::commands_for_energy;
use crate::estimator::{Settings, UpdateReport};
use crate::event_store::{CaptureMetadata, ClosedCapture, EventLog};
use crate::game_stats::{self, Activity, SessionTally};
use crate::live_map::{LiveMap, MapProgress};
use crate::map::PlanetMap;
use crate::map_change::{BoardChange, BoardModeDetector, BoundarySource, MapChangeDetector};
use crate::protocol::{ClientCommand, EventParser, RecordedEvent, ServerEvent};
use crate::rate_limit::ByteRateLimiter;
use crate::strategy::{BotState, MapBelief, TargetingPolicy, TargetingSettings};
use crate::tui;
use std::error::Error;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PROTOCOL_VERSION: u32 = 8;
const ENERGY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(200);
const MAP_QUIET_PERIOD: Duration = Duration::from_millis(300);
// A busy game queues paths faster than they are fitted; a 5 s wait stalled every bot for
// seconds (capture 55). Wait briefly, then aim with the latest published belief.
const MAX_MAP_WAIT: Duration = Duration::from_millis(500);

type ClientResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

/// How long each recent map's estimate may settle before the stop-time statistics.
const STOP_SETTLE: Duration = Duration::from_secs(1);

pub fn run(config: &AppConfig) -> ClientResult<()> {
    let running = Arc::new(AtomicBool::new(true));
    let signal_running = Arc::clone(&running);
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
        Some(Arc::new(Mutex::new(EventLog::create_session(
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
    let (map_feed, map_worker) = LiveMap::spawn(
        config.targeting && !config.dummy && !config.oracle_map_for_policy,
        Settings {
            planets: config.planets,
            learn_planets: config.learn_planets,
            ..Settings::default()
        },
    );
    let live_map = map_worker.is_some();
    if let Some(log) = &event_log {
        map_feed.set_capture(
            log.lock()
                .map_err(|_| "event log mutex poisoned")?
                .capture_id(),
        );
    }
    let tallies: Vec<Arc<Mutex<SessionTally>>> = names
        .iter()
        .map(|name| Arc::new(Mutex::new(SessionTally::new(name))))
        .collect();
    // What is in memory now (only the shot tallies without a live map); printed once. The lock
    // is held while printing, so a second Ctrl+C during the print exits after it.
    let printed = Arc::new(Mutex::new(false));
    let print_immediate = {
        let (map_feed, tallies, printed) =
            (map_feed.clone(), tallies.clone(), Arc::clone(&printed));
        move || {
            let mut printed = printed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if std::mem::replace(&mut *printed, true) {
                return Vec::new();
            }
            let maps = if live_map {
                map_feed.maps()
            } else {
                Vec::new()
            };
            let tallies: Vec<SessionTally> =
                tallies.iter().map(|t| t.lock().unwrap().clone()).collect();
            eprint!("{}", game_stats::immediate_report(&maps, &tallies));
            maps
        }
    };
    // First Ctrl+C: stop the sessions (no new shots), print the statistics, settle each recent
    // map, print it, exit. A second one prints the in-memory statistics if they are not out yet
    // and exits at once.
    // Stop requests so far: Ctrl+C presses and a lost server connection, which counts as the
    // first press.
    let stops = Arc::new(AtomicUsize::new(0));
    let signal_stops = Arc::clone(&stops);
    let print_on_exit = print_immediate.clone();
    // The signal handler; the live view calls it for a Ctrl+C key (raw mode has no SIGINT).
    let interrupt: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        tui::restore();
        if signal_stops.fetch_add(1, Ordering::SeqCst) == 0 {
            signal_running.store(false, Ordering::SeqCst);
            eprintln!("Stopping: statistics follow (Ctrl+C again to exit now)");
        } else {
            print_on_exit();
            eprintln!("Exiting now");
            std::process::exit(130);
        }
    });
    let signal_interrupt = Arc::clone(&interrupt);
    ctrlc::set_handler(move || signal_interrupt())?;
    // The live terminal view, when stdout is a terminal; stdout and stderr go to the run log
    // meanwhile. Dropped (terminal restored) before the statistics.
    let run_log = config.db_path.with_extension("log");
    let view = if config.tui && io::stdout().is_terminal() {
        let sources = tui::Sources {
            tallies: tallies.clone(),
            map: live_map.then(|| map_feed.clone()),
            stops: Arc::clone(&stops),
            interrupt,
        };
        match tui::start(sources, &run_log) {
            Ok(view) => Some(view),
            Err(error) => {
                eprintln!("Live view unavailable ({error}); plain logging");
                None
            }
        }
    } else {
        None
    };
    let mut workers = Vec::new();
    for (index, name) in names.into_iter().enumerate() {
        let config = config.clone();
        let running = Arc::clone(&running);
        let event_log = event_log.clone();
        let map_feed = map_feed.clone();
        let tally = Arc::clone(&tallies[index]);
        let stops = Arc::clone(&stops);
        workers.push(std::thread::spawn(move || {
            let result = run_worker(
                &config, index, &name, &running, event_log, &map_feed, &tally,
            );
            // A game that goes away (the server closed or dropped the connection while the run
            // was going) ends the run like a first Ctrl+C: statistics, settle, exit.
            let lost = match &result {
                Ok(()) => running.load(Ordering::SeqCst),
                Err(error) => connection_lost(error.as_ref()),
            };
            tally.lock().unwrap().activity = Activity::Stopped;
            if lost {
                tui::restore();
                if let Err(error) = &result {
                    eprintln!("{name}: server connection lost ({error})");
                }
                if stops.fetch_add(1, Ordering::SeqCst) == 0 {
                    eprintln!("Stopping: statistics follow (Ctrl+C to exit now)");
                }
            }
            if lost || result.is_err() {
                running.store(false, Ordering::SeqCst);
            }
            if lost { Ok(()) } else { result }
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
    if view.is_some() {
        drop(view);
        eprintln!("Run log: {}", run_log.display());
    }
    // Statistics: what is in memory now, then each recent map once it has settled.
    let maps = print_immediate();
    if let Some(map_worker) = map_worker {
        for settled in map_feed.settle(STOP_SETTLE) {
            let truth = maps
                .iter()
                .find(|m| m.epoch == settled.epoch)
                .and_then(|m| m.truth.as_deref());
            eprint!("{}", game_stats::settled_report(&settled, truth));
        }
        drop(map_feed);
        if map_worker.join().is_err() && worker_error.is_none() {
            worker_error = Some("map fitter worker panicked".into());
        }
    }
    let finish_result = if let Some(log) = &event_log {
        log.lock()
            .map_err(|_| "event log mutex poisoned")?
            .finish(now_unix_ms()?)
            .map(|closed| closed.iter().for_each(report_closed_capture))
    } else {
        Ok(())
    };
    if let Some(error) = worker_error {
        return Err(error);
    }
    finish_result?;
    Ok(())
}

/// Whether a session error means the server went away (the game ended or crashed).
fn connection_lost(error: &(dyn Error + Send + Sync + 'static)) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::UnexpectedEof
        )
    })
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
    tally: &Mutex<SessionTally>,
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
        tally,
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
    tally: &Mutex<SessionTally>,
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
    let mut pending_energy: Option<PendingEnergy> = None;
    let mut map_change = MapChangeDetector::default();
    let mut board = BoardModeDetector::default();
    // The first session records shared world events and owns the map fit and capture rollover;
    // every session detects boundaries itself for its own targeting state and firing pause.
    let canonical = bot_index == 0;
    let feeds_map = targeting && !dummy && !oracle_map_for_policy && canonical;
    'session: loop {
        if !running.load(Ordering::SeqCst) {
            break;
        }
        match stream.read(&mut buffer) {
            Ok(0) => {
                tui::note(format!("{name}: server closed the connection"));
                break;
            }
            Ok(length) => {
                for event in parser.feed(&buffer[..length])? {
                    if !running.load(Ordering::SeqCst) {
                        break 'session;
                    }
                    println!("{}", event_summary(&event));
                    let now_ms = now_unix_ms()?;
                    let boundary = map_change.observe(&event);
                    if let Some(change) = board.observe(&event, now_ms) {
                        report_board_change(&event_log, name, now_ms, change, "shot_finished")?;
                    }
                    tally.lock().unwrap().observe(
                        &event,
                        bot_state.own_id(),
                        bot_state.player_positions(),
                    );
                    bot_state.observe_at(&event, now_ms);
                    if let Some(source) = boundary {
                        // Shots, coverage and the belief of the old map no longer apply.
                        bot_state.start_new_map();
                        tally.lock().unwrap().new_map();
                        oracle_belief = None;
                        if let Some(change) = board.new_map() {
                            report_board_change(&event_log, name, now_ms, change, "new_map")?;
                        }
                        tui::note(format!(
                            "{name}: map boundary ({}): new map",
                            source.as_str()
                        ));
                        if canonical {
                            start_next_map(
                                feeds_map,
                                map_feed,
                                &bot_state,
                                &event_log,
                                now_ms,
                                source,
                                map_change.boundary_positions(),
                            )?;
                            if let Some(pending) = &mut pending_energy {
                                pending.last_map_event = Instant::now();
                            }
                        }
                    }
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
                    if feeds_map {
                        if let ServerEvent::Planets { planets } = &event {
                            // The disclosed planets of the current map (after a boundary that
                            // this event completed): statistics only.
                            map_feed.set_truth(planets.clone());
                        }
                    }
                    if feeds_map
                        && matches!(
                            event,
                            ServerEvent::ShotFin { .. }
                                | ServerEvent::PlayerPosition { .. }
                                | ServerEvent::PlayerLeave { .. }
                        )
                    {
                        map_feed.observe(&event)?;
                        if let Some(pending) = &mut pending_energy {
                            pending.last_map_event = Instant::now();
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
                        tally.lock().unwrap().energy = Some(energy);
                        // On the end card the server simulates nothing: send no commands.
                        let commands = if board.active() {
                            None
                        } else if dummy {
                            commands_for_energy(energy, &mut rand::rng())
                        } else if targeting && !oracle_map_for_policy {
                            let now = Instant::now();
                            let pending = pending_energy.get_or_insert(PendingEnergy {
                                energy,
                                started: now,
                                last_map_event: now,
                            });
                            pending.energy = energy;
                            None
                        } else if targeting {
                            tally.lock().unwrap().activity = Activity::Aiming;
                            let (progress, belief, report) = if oracle_map_for_policy {
                                (map_feed.progress(), oracle_belief.clone(), None)
                            } else {
                                map_feed.snapshot_with_progress()
                            };
                            target_commands(
                                energy,
                                &bot_state,
                                belief.as_ref(),
                                report.as_ref(),
                                progress,
                                false,
                                0,
                                &target_policy,
                                targeting_settings.angle_step_degrees,
                                &event_log,
                                name,
                                running,
                                if oracle_map_for_policy {
                                    "oracle"
                                } else {
                                    "online"
                                },
                            )?
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

        let now_ms = now_unix_ms()?;
        if let Some(change) = board.poll(now_ms) {
            report_board_change(&event_log, name, now_ms, change, "no_shot_finished")?;
            pending_energy = None;
        }

        if let Some(pending) = &pending_energy {
            let now = Instant::now();
            let progress = map_feed.progress();
            let (ready, fallback) = pending.map_wait_status(progress, now);
            if ready || fallback {
                let pending = pending_energy.take().unwrap();
                let (progress, belief, report) = map_feed.snapshot_with_progress();
                let wait_ms = pending.started.elapsed().as_millis() as u64;
                tally.lock().unwrap().activity = Activity::Aiming;
                let commands = target_commands(
                    pending.energy,
                    &bot_state,
                    belief.as_ref(),
                    report.as_ref(),
                    progress,
                    fallback,
                    wait_ms,
                    &target_policy,
                    targeting_settings.angle_step_degrees,
                    &event_log,
                    name,
                    running,
                    "online",
                )?;
                for command in commands.into_iter().flatten() {
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

        if pending_energy.is_none() && last_energy_request.elapsed() >= ENERGY_POLL_INTERVAL {
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
        tally.lock().unwrap().activity = if board.active() {
            Activity::BoardMode
        } else if pending_energy.is_some() {
            Activity::MapWait
        } else {
            Activity::WaitingEnergy
        };
    }

    // Closing TCP is sufficient to leave the game, and never waits for rate-limit credit.
    if !running.load(Ordering::SeqCst) {
        stream.shutdown(Shutdown::Both)?;
    }

    Ok(())
}

/// Map boundary on the canonical session: restart the live fit (keeping the tracked positions,
/// which upstream may send before the boundary is recognized) and roll the capture over.
fn start_next_map(
    feeds_map: bool,
    map_feed: &LiveMap,
    bot_state: &BotState,
    event_log: &Option<Arc<Mutex<EventLog>>>,
    now_ms: i64,
    source: BoundarySource,
    new_map_positions: &[(u32, f32, f32)],
) -> ClientResult<()> {
    if feeds_map {
        map_feed.reset();
        for (player_id, position) in bot_state.player_positions() {
            map_feed.observe(&ServerEvent::PlayerPosition {
                player_id: *player_id,
                x: position.x,
                y: position.y,
            })?;
        }
        eprintln!("LiveMap reset: new map epoch");
    }
    if let Some(log) = event_log {
        let mut log = log.lock().map_err(|_| "event log mutex poisoned")?;
        let closed = log.start_next_map(now_ms, source.as_str(), new_map_positions)?;
        report_closed_capture(&closed);
        tui::note(format!("Recording map in capture {}", log.capture_id()));
        map_feed.set_capture(log.capture_id());
    }
    Ok(())
}

fn report_closed_capture(closed: &ClosedCapture) {
    if closed.kept {
        eprintln!(
            "Closed capture {} ({} completed paths)",
            closed.capture_id, closed.completed_paths
        );
    } else {
        eprintln!(
            "Deleted capture {}: {} completed paths, fewer than {}",
            closed.capture_id,
            closed.completed_paths,
            crate::event_store::MIN_COMPLETED_PATHS
        );
    }
}

fn report_board_change(
    event_log: &Option<Arc<Mutex<EventLog>>>,
    name: &str,
    now_ms: i64,
    change: BoardChange,
    reason: &str,
) -> ClientResult<()> {
    let active = change == BoardChange::Entered;
    tui::note(format!(
        "{name}: {} ({reason})",
        if active {
            "board mode: pausing commands"
        } else {
            "board mode over: resuming"
        }
    ));
    if let Some(log) = event_log {
        log.lock()
            .map_err(|_| "event log mutex poisoned")?
            .append_diagnostic(
                now_ms,
                name,
                "board_mode",
                &serde_json::json!({"active": active, "reason": reason}),
            )?;
    }
    Ok(())
}

struct PendingEnergy {
    energy: f64,
    started: Instant,
    last_map_event: Instant,
}

impl PendingEnergy {
    fn map_wait_status(&self, progress: MapProgress, now: Instant) -> (bool, bool) {
        let ready = progress.processed >= progress.submitted
            && now.duration_since(self.last_map_event) >= MAP_QUIET_PERIOD;
        let fallback = !ready && now.duration_since(self.started) >= MAX_MAP_WAIT;
        (ready, fallback)
    }
}

fn target_commands(
    energy: f64,
    bot_state: &BotState,
    map_belief: Option<&MapBelief>,
    update_report: Option<&UpdateReport>,
    progress: MapProgress,
    fallback: bool,
    wait_ms: u64,
    policy: &TargetingPolicy,
    angle_step_degrees: u16,
    event_log: &Option<Arc<Mutex<EventLog>>>,
    name: &str,
    running: &AtomicBool,
    map_source: &str,
) -> ClientResult<Option<[ClientCommand; 2]>> {
    let mut state = bot_state.clone();
    state.set_map_belief(map_belief.cloned());
    state.set_clock_ms(now_unix_ms()?);
    let started = Instant::now();
    let outcome = policy.decide_while_running(&state, energy, running);
    let choice = outcome.choice;
    if !running.load(Ordering::SeqCst) {
        return Ok(None);
    }
    let search_duration_ms = started.elapsed().as_millis();
    if let Some(log) = event_log {
        let belief = map_belief.map(MapBelief::diagnostic_json);
        let decision = choice.map(|choice| {
            serde_json::json!({
                "reason": format!("{:?}", choice.reason),
                "velocity": choice.velocity,
                "angle_degrees": choice.angle_degrees,
                "miss_distance": choice.miss_distance,
                "predicted_hit": choice.predicted_hit,
                "predicted_victim": choice.predicted_victim,
                "information_score": choice.information_score,
                "uncertainty_available": choice.uncertainty_available,
                "covered_fraction": choice.covered_fraction,
                "contact_before_target": choice.contact_before_target,
                "victim_doom": choice.victim_doom,
                "doomed_hits_skipped": choice.doomed_hits_skipped,
                "victim_energy": choice.victim_energy,
                "victim_bounty": choice.victim_bounty,
                "flight_ticks": choice.flight_ticks,
            })
        });
        log.lock()
            .map_err(|_| "event log mutex poisoned")?
            .append_diagnostic(
                now_unix_ms()?,
                name,
                "target_decision",
                &serde_json::json!({
                    "energy": energy,
                    "map_epoch": progress.epoch,
                    "map_submitted_watermark": progress.submitted,
                    "map_processed_watermark": progress.processed,
                    "map_pending_events": progress.pending,
                    "map_pending_paths": progress.pending_paths,
                    "map_source": map_source,
                    "map_belief": belief,
                    "map_update_report": update_report,
                    "bot_state": bot_state.diagnostic_snapshot(),
                    "angle_step_degrees": angle_step_degrees,
                    "search_duration_ms": search_duration_ms,
                    "map_wait_ms": wait_ms,
                    "map_wait_fallback": fallback,
                    "choice": decision,
                    "decision": outcome.diagnostic_json(),
                }),
            )?;
    }
    Ok(choice.map(|choice| {
        println!("TargetChoice reason={:?} velocity={} angle={} miss={:.2} information={:.3} map_wait_ms={} fallback={}",
            choice.reason, choice.velocity, choice.angle_degrees, choice.miss_distance,
            choice.information_score, wait_ms, fallback);
        [
            ClientCommand::SetVelocity { velocity: f64::from(choice.velocity) },
            ClientCommand::SetAngle { angle: f64::from(choice.angle_degrees) },
        ]
    }))
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
    fn map_wait_allows_slow_fits_but_has_a_short_maximum() {
        let now = Instant::now();
        let pending = PendingEnergy {
            energy: 8.0,
            started: now,
            last_map_event: now,
        };
        let progress = MapProgress {
            epoch: 1,
            submitted: 2,
            processed: 1,
            pending: 1,
            pending_paths: 1,
        };

        assert_eq!(
            pending.map_wait_status(progress, now + MAX_MAP_WAIT - Duration::from_millis(1)),
            (false, false)
        );
        assert_eq!(
            pending.map_wait_status(progress, now + MAX_MAP_WAIT),
            (false, true)
        );

        let drained = MapProgress {
            submitted: progress.processed,
            pending: 0,
            ..progress
        };
        assert_eq!(
            pending.map_wait_status(drained, now + MAP_QUIET_PERIOD - Duration::from_millis(1)),
            (false, false)
        );
        assert_eq!(
            pending.map_wait_status(drained, now + MAP_QUIET_PERIOD),
            (true, false)
        );
        assert_eq!(
            pending.map_wait_status(drained, now + MAX_MAP_WAIT),
            (true, false)
        );
    }

    #[test]
    fn targeting_fires_with_the_latest_belief_after_a_bounded_map_wait() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::mpsc;
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (feed, fitter) = LiveMap::spawn_with(Settings::default(), move |_, _, _| {
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
            line.clear();
            reader.read_line(&mut line).unwrap(); // Initial energy request.
            let mut frames = Vec::new();
            frames.extend(1u32.to_ne_bytes());
            frames.extend(1u32.to_ne_bytes());
            for (id, x, y) in [(1u32, 100.0f32, 100.0f32), (2, 200.0, 100.0)] {
                frames.extend(3u32.to_ne_bytes());
                frames.extend(id.to_ne_bytes());
                frames.extend(x.to_ne_bytes());
                frames.extend(y.to_ne_bytes());
            }
            frames.extend(8u32.to_ne_bytes());
            frames.extend(1u32.to_ne_bytes());
            frames.extend(8f64.to_ne_bytes());
            socket.write_all(&frames).unwrap();
            let energy_sent = Instant::now();

            let mut shots = 0;
            while shots < 2 {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.trim().parse::<f64>().is_ok() || line.starts_with("v ") => {
                        shots += 1
                    }
                    _ => {}
                }
            }
            shots_tx.send((shots, energy_sent.elapsed())).unwrap();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
            }
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
                &Mutex::new(SessionTally::new("Einstein")),
            )
        });
        // The fit never finishes while the bot waits, so it fires after the bounded wait.
        let (shots, elapsed) = shots_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(shots, 2);
        assert!(elapsed >= MAX_MAP_WAIT, "fired after {elapsed:?}");
        release_tx.send(()).unwrap();
        running.store(false, Ordering::SeqCst);
        client.join().unwrap().unwrap();
        server.join().unwrap();
        drop(feed);
        fitter.join().unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let data: String = db.query_row(
            "SELECT data_json FROM diagnostics WHERE kind = 'target_decision' ORDER BY sequence LIMIT 1",
            [], |row| row.get(0),
        ).unwrap();
        let decision: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(decision["energy"], 8.0);
        assert_eq!(decision["map_submitted_watermark"], 1);
        assert_eq!(decision["map_processed_watermark"], 0);
        assert_eq!(decision["map_pending_paths"], 0);
        // The fake fit reports no update; the key is still recorded (null, not omitted).
        assert!(decision["map_update_report"].is_null());
        assert!(
            decision
                .as_object()
                .unwrap()
                .contains_key("map_update_report")
        );
        assert_eq!(decision["map_wait_fallback"], true);
        assert!(decision["map_wait_ms"].as_u64().unwrap() >= 500);
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
            let (feed, fitter) = LiveMap::spawn(false, Settings::default());
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
                &Mutex::new(SessionTally::new("Einstein")),
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
    fn end_card_pauses_commands_and_a_new_map_starts_a_new_capture() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::mpsc;
        fn frame(words: &[[u8; 4]]) -> Vec<u8> {
            words.concat()
        }
        let u = |value: u32| value.to_ne_bytes();
        let f = |value: f32| value.to_ne_bytes();
        let position = move |id, x, y| frame(&[u(3), u(id), f(x), f(y)]);
        let energy = move || [u(8).to_vec(), u(1).to_vec(), 8f64.to_ne_bytes().to_vec()].concat();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (lines_tx, lines_rx) = mpsc::channel::<String>();
        let (phase_tx, phase_rx) = mpsc::channel::<&str>();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.write_all(b"welcome> ").unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            socket.write_all(b"> ").unwrap();
            std::thread::spawn(move || {
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if lines_tx.send(line).is_err() {
                        break;
                    }
                }
            });
            let mut frames = frame(&[u(1), u(1)]);
            frames.extend(position(1, 100.0, 100.0));
            frames.extend(position(2, 300.0, 300.0));
            for _ in 0..3 {
                // ShotFin of player 2 ending far from everyone.
                frames.extend(frame(&[u(6), u(2)]));
                frames.extend([0f64.to_ne_bytes(), 5f64.to_ne_bytes()].concat());
                frames.extend(frame(&[u(1), f(900.0), f(900.0)]));
            }
            for _ in 0..3 {
                frames.extend(frame(&[u(5), u(2)]));
                frames.extend([0f64.to_ne_bytes(), 5f64.to_ne_bytes()].concat());
            }
            socket.write_all(&frames).unwrap();
            // No shot finishes any more: the end card.
            let quiet = crate::map_change::BOARD_QUIET_MS as u64 + 400;
            std::thread::sleep(Duration::from_millis(quiet));
            phase_tx.send("end card").unwrap();
            socket.write_all(&energy()).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            phase_tx.send("new map").unwrap();
            let mut burst = position(1, 500.0, 500.0);
            burst.extend(position(2, 700.0, 100.0));
            socket.write_all(&burst).unwrap();
            socket.write_all(&energy()).unwrap();
            std::thread::sleep(Duration::from_millis(600));
            socket
        });
        let path = std::env::temp_dir().join(format!(
            "einsteinbot-end-card-{}-{}.sqlite",
            std::process::id(),
            now_unix_ms().unwrap()
        ));
        let log = Arc::new(Mutex::new(
            EventLog::create_session(
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
        let first_capture = log.lock().unwrap().capture_id();
        let running = Arc::new(AtomicBool::new(true));
        let worker_running = Arc::clone(&running);
        let worker_log = Arc::clone(&log);
        let client = std::thread::spawn(move || {
            let (feed, _) = LiveMap::spawn(false, Settings::default());
            let mut socket = TcpStream::connect(address).unwrap();
            run_session(
                &mut socket,
                "Einstein",
                0,
                true,
                false,
                false,
                &worker_running,
                Some(worker_log),
                &feed,
                &Mutex::new(SessionTally::new("Einstein")),
            )
        });
        assert_eq!(
            phase_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "end card"
        );
        assert_eq!(
            phase_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            "new map"
        );
        let _socket = server.join().unwrap();
        running.store(false, Ordering::SeqCst);
        client.join().unwrap().unwrap();
        let lines: Vec<String> = lines_rx.try_iter().collect();
        let commands: Vec<&str> = lines
            .iter()
            .map(|line| line.trim())
            .filter(|line| *line != "u" && !line.starts_with("b "))
            .collect();
        // One velocity + angle pair, sent after the new map's energy, none on the end card.
        assert_eq!(commands.len(), 2, "{lines:?}");
        assert!(commands[0].starts_with("v "));

        let db = rusqlite::Connection::open(&path).unwrap();
        let second_capture = log.lock().unwrap().capture_id();
        assert_ne!(second_capture, first_capture);
        let count = |capture: i64, kind: &str| -> i64 {
            db.query_row(
                "SELECT COUNT(*) FROM events WHERE capture_id = ?1 AND event_type = ?2",
                rusqlite::params![capture, kind],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(count(first_capture, "set_velocity"), 0);
        assert_eq!(count(second_capture, "set_velocity"), 1);
        assert_eq!(count(first_capture, "shot_fin"), 3);
        let board: String = db
            .query_row(
                "SELECT data_json FROM diagnostics WHERE capture_id = ?1 AND kind = 'board_mode'",
                [first_capture],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&board).unwrap()["active"],
            true
        );
        let metadata: String = db
            .query_row(
                "SELECT metadata_json FROM captures WHERE id = ?1",
                [second_capture],
                |row| row.get(0),
            )
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(
            metadata["map"]["boundary_source"],
            "all_players_repositioned"
        );
        assert_eq!(metadata["session"]["map_index"], 1);
        // The new capture starts with the new map's positions, not the old map's.
        let initial: Vec<String> = db
            .prepare(
                "SELECT event_json FROM events WHERE capture_id = ?1 AND event_type = 'player_position' ORDER BY sequence LIMIT 2",
            )
            .unwrap()
            .query_map([second_capture], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let initial: Vec<(f64, f64)> = initial
            .iter()
            .map(|text| {
                let event: serde_json::Value = serde_json::from_str(text).unwrap();
                (
                    event["event"]["x"].as_f64().unwrap(),
                    event["event"]["y"].as_f64().unwrap(),
                )
            })
            .collect();
        assert_eq!(initial, [(500.0, 500.0), (700.0, 100.0)]);
        drop(db);
        drop(log);
        std::fs::remove_file(path).unwrap();
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
    fn a_dropped_server_connection_counts_as_a_stop_not_a_failure() {
        let boxed = |error: Box<dyn Error + Send + Sync>| connection_lost(error.as_ref());
        assert!(boxed(Box::new(io::Error::from(io::ErrorKind::BrokenPipe))));
        assert!(boxed(Box::new(io::Error::from(
            io::ErrorKind::ConnectionReset
        ))));
        assert!(!boxed(Box::new(io::Error::from(
            io::ErrorKind::PermissionDenied
        ))));
        assert!(!boxed("event log mutex poisoned".into()));
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
            planets: None,
            learn_planets: false,
            tui: false,
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
