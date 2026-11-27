//! Live terminal view of a run (P22): per bot its energy, shots, hits and activity, the current
//! map's live estimate figures, and a few important events. It runs on its own thread and only
//! reads small snapshots under the locks the sessions and the live map already hold (the shot
//! tallies, `LiveMap::status`); it never calls the estimator or simulates.
//!
//! While the view is up the terminal is in raw mode on the alternate screen, and the process's
//! stdout and stderr (fd 1 and 2) point to a log file, so the existing plain logging cannot
//! corrupt the screen. `restore` undoes both; it is idempotent and runs on every exit path (the
//! first stop request, a second Ctrl+C, a panic, dropping `Tui`), always before the stop-time
//! statistics are printed. In raw mode Ctrl+C is a key, not a signal: the view routes it to the
//! same interrupt handler as the signal.
use crate::game_stats::{self, Activity, SessionTally};
use crate::live_map::{LiveMap, MapStatus};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::{cursor, execute, terminal};
use ratatui::layout::{Constraint, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Redraw period (4 Hz); also the longest a stop request waits for the view to go away.
const REFRESH: Duration = Duration::from_millis(250);
/// Events kept for the log area.
const LOG_LINES: usize = 64;
/// Size at which the run log is rotated (checked at start and on every redraw): it becomes
/// `<log>.1`, replacing the previous one, and a new log is started. At most twice this is kept.
const LOG_CAP: u64 = 32 << 20;

/// The original stdout and stderr while the view is up (`None` otherwise).
static SAVED: Mutex<Option<Saved>> = Mutex::new(None);
static LOG: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

struct Saved {
    out: RawFd,
    err: RawFd,
}

fn saved() -> MutexGuard<'static, Option<Saved>> {
    SAVED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An important event (map change, end card, connection loss): logged as before, and shown in
/// the view's log area while it is up.
pub fn note(message: impl std::fmt::Display) {
    let line = message.to_string();
    eprintln!("{line}");
    if saved().is_some() {
        let mut log = LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if log.len() == LOG_LINES {
            log.pop_front();
        }
        log.push_back(line);
    }
}

/// Leave the view: back to the normal screen and cooked mode, stdout and stderr to their
/// original targets. Does nothing when the view is not up.
pub fn restore() {
    let mut saved = saved();
    let Some(Saved { out, err }) = saved.take() else {
        return;
    };
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    // SAFETY: `out` and `err` are descriptors this module duplicated and still owns.
    unsafe {
        libc::dup2(out, libc::STDOUT_FILENO);
        libc::dup2(err, libc::STDERR_FILENO);
        libc::close(out);
        libc::close(err);
    }
    let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

/// What the view reads; all of it is shared with the sessions and the live map.
pub struct Sources {
    pub tallies: Vec<Arc<Mutex<SessionTally>>>,
    /// The live map, when the run fits one.
    pub map: Option<LiveMap>,
    /// Stop requests so far (Ctrl+C presses and lost connections); the view ends at the first.
    pub stops: Arc<AtomicUsize>,
    /// The Ctrl+C handler, called for a Ctrl+C key after the terminal is restored.
    pub interrupt: Arc<dyn Fn() + Send + Sync>,
}

/// The running view; dropping it ends the view and restores the terminal.
pub struct Tui {
    quit: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Tui {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        restore();
    }
}

/// The rotated run log: `<log>.1`.
fn rotated(log_path: &Path) -> PathBuf {
    let mut name = log_path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// If the log at `log_path` has reached `cap` bytes, move it to `<log>.1` (replacing an older
/// one) and return a new, empty log there; `None` while it is below the cap.
fn rotate_if_full(log_path: &Path, cap: u64) -> io::Result<Option<File>> {
    if std::fs::metadata(log_path).map_or(0, |m| m.len()) < cap {
        return Ok(None);
    }
    std::fs::rename(log_path, rotated(log_path))?;
    File::options()
        .create(true)
        .append(true)
        .open(log_path)
        .map(Some)
}

/// Point stdout and stderr at `log`.
fn redirect(log: &File) {
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    // SAFETY: `log` is an open file; fd 1 and 2 refer to it until the next `redirect` or
    // `restore`.
    unsafe {
        libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
    }
}

/// Start the view, sending stdout and stderr to `log_path` (appended; rotated at `LOG_CAP`)
/// meanwhile.
pub fn start(sources: Sources, log_path: &Path) -> io::Result<Tui> {
    let log = match rotate_if_full(log_path, LOG_CAP)? {
        Some(log) => log,
        None => File::options().create(true).append(true).open(log_path)?,
    };
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();
    // SAFETY: duplicating the process's own standard descriptors; each result is checked.
    let (out, err, screen) = unsafe {
        let out = libc::dup(libc::STDOUT_FILENO);
        let err = libc::dup(libc::STDERR_FILENO);
        let screen = libc::dup(libc::STDOUT_FILENO);
        if out < 0 || err < 0 || screen < 0 {
            return Err(io::Error::last_os_error());
        }
        (out, err, File::from_raw_fd(screen))
    };
    let mut screen = BufWriter::new(screen);
    *saved() = Some(Saved { out, err });
    if let Err(error) = terminal::enable_raw_mode() {
        restore();
        return Err(error);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let entered = execute!(screen, terminal::EnterAlternateScreen, cursor::Hide);
    redirect(&log);
    drop(log);
    let log_path = log_path.to_owned();
    let mut terminal = match entered.and_then(|()| Terminal::new(CrosstermBackend::new(screen))) {
        Ok(terminal) => terminal,
        Err(error) => {
            restore();
            return Err(error);
        }
    };
    let started = Instant::now();
    let quit = Arc::new(AtomicBool::new(false));
    let thread_quit = Arc::clone(&quit);
    let thread = std::thread::Builder::new()
        .name("tui".to_owned())
        .spawn(move || {
            while !thread_quit.load(Ordering::SeqCst) && sources.stops.load(Ordering::SeqCst) == 0 {
                {
                    // Draw only while the view is up, so no frame lands on the restored screen.
                    let up = saved();
                    if up.is_none() {
                        break;
                    }
                    // Under the lock, so a rotation never redirects a restored stdout.
                    if let Ok(Some(log)) = rotate_if_full(&log_path, LOG_CAP) {
                        redirect(&log);
                    }
                    let view = snapshot(&sources, started.elapsed());
                    let log: Vec<String> = LOG
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .iter()
                        .cloned()
                        .collect();
                    if terminal.draw(|frame| draw(frame, &view, &log)).is_err() {
                        break;
                    }
                }
                match event::poll(REFRESH) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }
                if is_ctrl_c(event::read()) {
                    restore();
                    (sources.interrupt)();
                    // Presses read along with this one, before the restore made Ctrl+C a
                    // signal again, count too.
                    while let Ok(true) = event::poll(Duration::ZERO) {
                        if is_ctrl_c(event::read()) {
                            (sources.interrupt)();
                        }
                    }
                }
            }
            restore();
            let wall = started.elapsed().as_secs_f64();
            let cpu = thread_cpu_seconds();
            eprintln!(
                "Live view: thread CPU {cpu:.2} s over {wall:.0} s ({:.2}% of a core)",
                100.0 * cpu / wall.max(1e-9)
            );
        })
        .inspect_err(|_| restore())?;
    Ok(Tui {
        quit,
        thread: Some(thread),
    })
}

fn is_ctrl_c(event: io::Result<Event>) -> bool {
    matches!(event, Ok(Event::Key(key)) if key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c'))
}

/// CPU time of the calling thread.
fn thread_cpu_seconds() -> f64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a valid out pointer.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) };
    time.tv_sec as f64 + time.tv_nsec as f64 * 1e-9
}

fn draw(frame: &mut ratatui::Frame, view: &View, log: &[String]) {
    let status = status_lines(view);
    let [top, bottom] = Layout::vertical([
        Constraint::Length(status.len() as u16 + 2),
        Constraint::Min(3),
    ])
    .areas(frame.area());
    let lines: Vec<Line> = status.into_iter().map(Line::from).collect();
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Einsteinbot  (Ctrl+C: stop) ")),
        top,
    );
    let shown = usize::from(bottom.height.saturating_sub(2));
    let tail: Vec<Line> = log[log.len().saturating_sub(shown)..]
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    frame.render_widget(
        Paragraph::new(tail).block(Block::bordered().title(" events ")),
        bottom,
    );
}

/// One bot's row.
#[derive(Clone, Debug, PartialEq)]
pub struct BotView {
    pub name: String,
    pub energy: Option<f64>,
    pub shots: u32,
    pub hits: u32,
    pub activity: Activity,
}

/// Everything one frame shows except the log.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub run_elapsed: Duration,
    pub bots: Vec<BotView>,
    /// The current map's live figures; `None` when the run fits no live map.
    pub map: Option<MapView>,
}

#[derive(Clone, Debug, Default)]
pub struct MapView {
    pub status: MapStatus,
    /// Time since the live map started recording this epoch.
    pub elapsed: Option<Duration>,
}

fn snapshot(sources: &Sources, run_elapsed: Duration) -> View {
    let bots = sources
        .tallies
        .iter()
        .map(|tally| {
            let tally = tally
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let game = tally.current_game();
            BotView {
                name: tally.name.clone(),
                energy: tally.energy,
                shots: game.shots,
                hits: game.hits,
                activity: tally.activity,
            }
        })
        .collect();
    let map = sources.map.as_ref().map(|map| {
        let status = map.status();
        MapView {
            elapsed: status.started.map(|s| s.elapsed()),
            status,
        }
    });
    View {
        run_elapsed,
        bots,
        map,
    }
}

fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The status area's text, one string per line (pure; tested without a terminal).
pub fn status_lines(view: &View) -> Vec<String> {
    let mut lines = vec![format!("run {}", clock(view.run_elapsed))];
    match &view.map {
        None => lines.push("map: no live map (targeting off, dummy or oracle map)".to_owned()),
        Some(map) => {
            let status = &map.status;
            let mut line = format!(
                "map {}  elapsed {}",
                status.epoch,
                map.elapsed.map_or("-".to_owned(), clock)
            );
            match &status.summary {
                Some(summary) => {
                    let _ = write!(line, "  paths {}  K {}", summary.paths, summary.planets);
                }
                None => line.push_str("  paths 0  K -"),
            }
            if let Some(disclosed) = status.disclosed {
                let _ = write!(line, " (disclosed {disclosed})");
            }
            if let Some(limit) = status
                .summary
                .as_ref()
                .map(|summary| &summary.count_limit)
                .filter(|limit| limit.limited())
            {
                let _ = write!(line, "  COUNT LIMITED (cap {}: more planets?)", limit.cap);
            }
            lines.push(line);
            lines.push(format!(
                "  queued paths {}  publish lag last {:.0} ms max {:.0} ms  search {}",
                status.pending_paths,
                status.lag.last_ms,
                status.lag.max_ms,
                status
                    .summary
                    .as_ref()
                    .map_or("not started".to_owned(), game_stats::search_state)
            ));
        }
    }
    lines.push(format!(
        "{:<15} {:>7} {:>6} {:>5}  state",
        "bot", "energy", "shots", "hits"
    ));
    for bot in &view.bots {
        lines.push(format!(
            "{:<15} {:>7} {:>6} {:>5}  {}",
            bot.name,
            bot.energy.map_or("-".to_owned(), |e| format!("{e:.1}")),
            bot.shots,
            bot.hits,
            bot.activity.as_str()
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_stats::Lag;

    #[test]
    fn a_full_run_log_is_rotated_into_one_older_file() {
        let dir = std::env::temp_dir().join(format!("einsteinbot-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("run.log");
        let read = |path: &Path| std::fs::read_to_string(path).unwrap();
        std::fs::write(&log, "first").unwrap();
        assert!(rotate_if_full(&log, 6).unwrap().is_none());
        assert_eq!(read(&log), "first");
        std::fs::write(&log, "second").unwrap();
        let mut fresh = rotate_if_full(&log, 6).unwrap().expect("at the cap");
        fresh.write_all(b"third").unwrap();
        assert_eq!(
            (read(&log), read(&rotated(&log))),
            ("third".into(), "second".into())
        );
        std::fs::write(&log, "fourth").unwrap();
        rotate_if_full(&log, 6).unwrap().expect("at the cap");
        assert_eq!(
            (read(&log), read(&rotated(&log))),
            ("".into(), "fourth".into())
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn status_lines_show_map_figures_and_one_row_per_bot() {
        let view = View {
            run_elapsed: Duration::from_secs(75),
            bots: vec![
                BotView {
                    name: "Einstein".to_owned(),
                    energy: Some(12.34),
                    shots: 7,
                    hits: 2,
                    activity: Activity::Aiming,
                },
                BotView {
                    name: "Curie".to_owned(),
                    energy: None,
                    shots: 0,
                    hits: 0,
                    activity: Activity::BoardMode,
                },
            ],
            map: Some(MapView {
                status: MapStatus {
                    epoch: 3,
                    disclosed: Some(24),
                    pending_paths: 2,
                    lag: Lag {
                        paths: 5,
                        last_ms: 120.4,
                        max_ms: 900.0,
                    },
                    ..MapStatus::default()
                },
                elapsed: Some(Duration::from_secs(61)),
            }),
        };
        let lines = status_lines(&view);
        assert_eq!(lines[0], "run 1:15");
        assert_eq!(lines[1], "map 3  elapsed 1:01  paths 0  K - (disclosed 24)");
        assert_eq!(
            lines[2],
            "  queued paths 2  publish lag last 120 ms max 900 ms  search not started"
        );
        assert_eq!(lines[4], "Einstein           12.3      7     2  aiming");
        assert_eq!(lines[5], "Curie                 -      0     0  board mode");
        assert_eq!(lines.len(), 6);
    }

    #[test]
    fn status_lines_warn_when_the_planet_count_is_limited() {
        let mut summary = crate::estimator::Estimator::new(crate::estimator::Settings {
            planets: Some(22),
            ..Default::default()
        })
        .summary();
        summary.count_limit.detected_at_path = Some(40);
        let view = View {
            map: Some(MapView {
                status: MapStatus {
                    summary: Some(summary),
                    ..MapStatus::default()
                },
                elapsed: None,
            }),
            ..View::default()
        };
        assert_eq!(
            status_lines(&view)[1],
            "map 0  elapsed -  paths 0  K 0  COUNT LIMITED (cap 22: more planets?)"
        );
    }

    #[test]
    fn status_lines_without_live_map_say_so() {
        let lines = status_lines(&View::default());
        assert_eq!(
            lines[1],
            "map: no live map (targeting off, dummy or oracle map)"
        );
    }
}
