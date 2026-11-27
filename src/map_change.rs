//! Map-change and board-mode detection from the event stream alone.
//!
//! NewtonWars ends every round with a board phase (the end card): `simulate()` stops, so no
//! trajectory finishes, while energy still grows and valid shots still launch. Then
//! `reinitialize()` draws new planets and repositions every active player in one server step,
//! sending each position (and, with `sendplanets 1`, the recipient's `Planets` among them) with
//! nothing else in between. Competitive servers send no `Planets`, so the detector recognizes
//! the reposition burst itself.
use crate::protocol::ServerEvent;
use std::collections::{BTreeMap, BTreeSet};

/// A shot ending within this distance of a player's previous position explains that player's
/// reposition as a kill (upstream `playerSize` is at most 10).
const KILL_RADIUS: f32 = 12.0;

/// Board mode is assumed once this many shots began without any shot finishing since, the
/// oldest of them at least `BOARD_QUIET_MS` ago. Measured on the recorded captures (P19).
pub const BOARD_MIN_UNFINISHED: usize = 3;
pub const BOARD_QUIET_MS: i64 = 1_500;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundarySource {
    /// A `Planets` message after the connection's initial one.
    Planets,
    /// Every known player (at least two) repositioned in one uninterrupted run of positions.
    AllPlayersRepositioned,
    /// The only known player repositioned without a shot ending at its old position.
    SoloReposition,
}

impl BoundarySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planets => "planets",
            Self::AllPlayersRepositioned => "all_players_repositioned",
            Self::SoloReposition => "solo_reposition",
        }
    }
}

/// One uninterrupted run of `PlayerPosition`/`Planets` events, the shape of a reinitialization.
struct Run {
    /// Players known when the run began; a boundary needs all of them repositioned.
    known_at_start: BTreeMap<u32, (f32, f32)>,
    repositioned: BTreeSet<u32>,
    /// Positions received in this run: the new map's, once the run is a boundary.
    positions: BTreeMap<u32, (f32, f32)>,
    emitted: bool,
    planets_seen: bool,
}

/// Detects the start of a new map. Feed every inbound event of one connection in order; the
/// result is `Some` exactly once per map change, at the event that completes the evidence.
#[derive(Default)]
pub struct MapChangeDetector {
    known: BTreeMap<u32, (f32, f32)>,
    run: Option<Run>,
    /// End point of the latest finished shot while only `ShotBegin`s (kill debris) followed it.
    last_shot_end: Option<(f32, f32)>,
    boundary_positions: Vec<(u32, f32, f32)>,
}

impl MapChangeDetector {
    /// Player positions of the new map received up to (and including) the event that
    /// completed the latest boundary. With a `Planets` boundary, players after the recipient
    /// in server order follow it; until then their known positions are the old map's.
    pub fn boundary_positions(&self) -> &[(u32, f32, f32)] {
        &self.boundary_positions
    }

    pub fn observe(&mut self, event: &ServerEvent) -> Option<BoundarySource> {
        match event {
            ServerEvent::PlayerPosition { player_id, x, y } => {
                let shot_end = self.last_shot_end.take();
                let sighting = !self.known.contains_key(player_id);
                let run = current_run(&mut self.run, &self.known, sighting);
                let position = (*x, *y);
                if let Some(old) = run.known_at_start.get(player_id) {
                    let killed = shot_end
                        .is_some_and(|end| (end.0 - old.0).hypot(end.1 - old.1) <= KILL_RADIUS);
                    if *old != position && !killed {
                        run.repositioned.insert(*player_id);
                    }
                }
                self.known.insert(*player_id, position);
                run.positions.insert(*player_id, position);
                let all = !run.known_at_start.is_empty()
                    && run
                        .known_at_start
                        .keys()
                        .all(|id| run.repositioned.contains(id));
                if all && !run.emitted {
                    run.emitted = true;
                    self.boundary_positions = flatten(&run.positions);
                    return Some(if run.known_at_start.len() >= 2 {
                        BoundarySource::AllPlayersRepositioned
                    } else {
                        BoundarySource::SoloReposition
                    });
                }
                None
            }
            ServerEvent::Planets { .. } => {
                self.last_shot_end = None;
                let run = current_run(&mut self.run, &self.known, false);
                // The connection's first `Planets` (handshake) arrives before any position.
                if run.known_at_start.is_empty() {
                    run.planets_seen = true;
                    return None;
                }
                if run.emitted && !run.planets_seen {
                    // The burst already completed; this is the same reinitialization's map.
                    run.planets_seen = true;
                    return None;
                }
                // A new map (also a second reinitialization right after the first one).
                self.boundary_positions = flatten(&run.positions);
                *run = Run {
                    known_at_start: self.known.clone(),
                    repositioned: BTreeSet::new(),
                    positions: BTreeMap::new(),
                    emitted: true,
                    planets_seen: true,
                };
                Some(BoundarySource::Planets)
            }
            ServerEvent::ShotFin { points, .. } | ServerEvent::ShotFinished { points, .. } => {
                self.run = None;
                self.last_shot_end = points.last().map(|point| (point.x, point.y));
                None
            }
            ServerEvent::ShotBegin { .. } => {
                // Kill debris launches between the killing shot's end and the victim's position.
                self.run = None;
                None
            }
            ServerEvent::PlayerLeave { player_id } => {
                self.run = None;
                self.last_shot_end = None;
                self.known.remove(player_id);
                None
            }
            ServerEvent::OwnId { .. }
            | ServerEvent::GameMode { .. }
            | ServerEvent::OwnEnergy { .. } => {
                self.run = None;
                self.last_shot_end = None;
                None
            }
        }
    }
}

/// The run this position/planets event belongs to. A run that began with no known player (the
/// connection's handshake: `Planets`, then first sightings) cannot be a boundary; it adopts the
/// players known by its first event that is not a first sighting (`sighting` is false).
fn current_run<'a>(
    run: &'a mut Option<Run>,
    known: &BTreeMap<u32, (f32, f32)>,
    sighting: bool,
) -> &'a mut Run {
    let run = run.get_or_insert_with(|| Run {
        known_at_start: known.clone(),
        repositioned: BTreeSet::new(),
        positions: BTreeMap::new(),
        emitted: false,
        planets_seen: false,
    });
    if run.known_at_start.is_empty() && !sighting {
        run.known_at_start = known.clone();
        run.positions.clear();
    }
    run
}

fn flatten(positions: &BTreeMap<u32, (f32, f32)>) -> Vec<(u32, f32, f32)> {
    positions.iter().map(|(&id, &(x, y))| (id, x, y)).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BoardChange {
    Entered,
    Left,
}

/// Detects the end card (board mode) from trajectories that stop finishing for ALL players.
/// Single shots vanish without a finish during play too (a hit player's debris overwrites its
/// shot slots, `spawnDebris`), so only a global stop counts. Shots still begin on the end card
/// (`stepSimulation` launches valid shots in either mode) and energy still grows, so neither
/// begun shots nor energy separates the phases. A false detection only pauses commands until
/// the next finished shot.
#[derive(Default)]
pub struct BoardModeDetector {
    /// Shots begun since the latest finished shot, kill debris excluded: (player, time ms).
    unfinished: Vec<(u32, i64)>,
    /// Only `ShotBegin`s followed the latest finished shot: a kill's debris, if the victim's
    /// position comes next.
    after_finish: bool,
    active: bool,
}

impl BoardModeDetector {
    pub fn active(&self) -> bool {
        self.active
    }

    pub fn observe(&mut self, event: &ServerEvent, now_ms: i64) -> Option<BoardChange> {
        match event {
            ServerEvent::ShotBegin { player_id, .. } => {
                self.unfinished.push((*player_id, now_ms));
                return None;
            }
            ServerEvent::ShotFin { .. } | ServerEvent::ShotFinished { .. } => {
                self.unfinished.clear();
                self.after_finish = true;
                return self.leave();
            }
            // Kill: the shot ends, the victim's debris launches, the victim respawns. There
            // are no kills on the end card, so debris is no evidence for it.
            ServerEvent::PlayerPosition { player_id, .. }
                if self.after_finish && self.unfinished.iter().all(|(id, _)| id == player_id) =>
            {
                self.unfinished.clear();
            }
            // A leaving player's shots stop being simulated and never finish.
            ServerEvent::PlayerLeave { player_id } => {
                self.unfinished.retain(|(id, _)| id != player_id);
            }
            _ => {}
        }
        self.after_finish = false;
        None
    }

    /// Re-evaluate the quiet period; call regularly, also without new events.
    pub fn poll(&mut self, now_ms: i64) -> Option<BoardChange> {
        let quiet = self.unfinished.len() >= BOARD_MIN_UNFINISHED
            && self
                .unfinished
                .first()
                .is_some_and(|(_, began)| now_ms - began >= BOARD_QUIET_MS);
        if quiet && !self.active {
            self.active = true;
            return Some(BoardChange::Entered);
        }
        None
    }

    /// A new map started: shots in flight were cleared by the server.
    pub fn new_map(&mut self) -> Option<BoardChange> {
        self.unfinished.clear();
        self.leave()
    }

    fn leave(&mut self) -> Option<BoardChange> {
        std::mem::replace(&mut self.active, false).then_some(BoardChange::Left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Point;

    fn pos(player_id: u32, x: f32, y: f32) -> ServerEvent {
        ServerEvent::PlayerPosition { player_id, x, y }
    }

    fn fin(player_id: u32, end: (f32, f32)) -> ServerEvent {
        ServerEvent::ShotFin {
            player_id,
            angle: 0.0,
            velocity: 10.0,
            points: vec![Point { x: 0.0, y: 0.0 }, Point { x: end.0, y: end.1 }],
        }
    }

    fn begin(player_id: u32) -> ServerEvent {
        ServerEvent::ShotBegin {
            player_id,
            angle: 0.0,
            velocity: 10.0,
        }
    }

    fn feed(detector: &mut MapChangeDetector, events: &[ServerEvent]) -> Vec<BoundarySource> {
        events.iter().filter_map(|e| detector.observe(e)).collect()
    }

    fn handshake(players: &[(u32, f32, f32)]) -> Vec<ServerEvent> {
        let mut events = vec![
            ServerEvent::GameMode {
                player_id: 1,
                mode: 1,
            },
            ServerEvent::OwnId { player_id: 1 },
            ServerEvent::Planets { planets: vec![] },
        ];
        events.extend(players.iter().map(|&(id, x, y)| pos(id, x, y)));
        events
    }

    #[test]
    fn handshake_and_joins_are_not_boundaries() {
        let mut detector = MapChangeDetector::default();
        let mut events = handshake(&[(1, 10.0, 10.0), (2, 500.0, 500.0)]);
        events.push(ServerEvent::OwnEnergy {
            player_id: 1,
            energy: 20.0,
        });
        events.push(pos(3, 900.0, 100.0)); // a new player joins
        assert!(feed(&mut detector, &events).is_empty());
    }

    #[test]
    fn all_players_repositioning_is_one_boundary_even_split_across_reads() {
        let mut detector = MapChangeDetector::default();
        feed(
            &mut detector,
            &handshake(&[(1, 10.0, 10.0), (2, 500.0, 500.0)]),
        );
        feed(&mut detector, &[fin(2, (300.0, 300.0))]);
        // The burst is fed in two separate calls, as two TCP reads would deliver it.
        assert_eq!(detector.observe(&pos(1, 100.0, 200.0)), None);
        assert_eq!(
            detector.observe(&pos(2, 700.0, 50.0)),
            Some(BoundarySource::AllPlayersRepositioned)
        );
        assert_eq!(
            detector.boundary_positions(),
            [(1, 100.0, 200.0), (2, 700.0, 50.0)]
        );
        // Our own Planets, sent in the same burst, is the same boundary.
        assert_eq!(
            detector.observe(&ServerEvent::Planets { planets: vec![] }),
            None
        );
    }

    #[test]
    fn planets_inside_the_burst_triggers_once() {
        let mut detector = MapChangeDetector::default();
        feed(
            &mut detector,
            &handshake(&[(1, 10.0, 10.0), (2, 500.0, 500.0)]),
        );
        let burst = [
            pos(1, 100.0, 200.0),
            ServerEvent::Planets { planets: vec![] },
            pos(2, 700.0, 50.0),
        ];
        assert_eq!(feed(&mut detector, &burst), vec![BoundarySource::Planets]);
        // Only the position sent before our Planets is known to belong to the new map.
        assert_eq!(detector.boundary_positions(), [(1, 100.0, 200.0)]);
        // An immediate second reinitialization is a new map again.
        assert_eq!(
            feed(&mut detector, &burst[..2]),
            vec![BoundarySource::Planets]
        );
    }

    #[test]
    fn a_kill_or_an_interrupted_run_is_not_a_boundary() {
        let mut detector = MapChangeDetector::default();
        feed(
            &mut detector,
            &handshake(&[(1, 10.0, 10.0), (2, 500.0, 500.0)]),
        );
        // Player 1 kills player 2: shot end, debris, victim respawn. Then 1 dies the same way.
        let events = [
            fin(1, (501.0, 499.0)),
            begin(2),
            begin(2),
            pos(2, 800.0, 800.0),
            fin(2, (11.0, 10.0)),
            begin(1),
            pos(1, 50.0, 60.0),
        ];
        assert!(feed(&mut detector, &events).is_empty());
        // A respawn right before a reinitialization does not count toward its burst.
        let events = [
            fin(1, (801.0, 800.0)),
            pos(2, 300.0, 300.0),
            pos(1, 900.0, 900.0),
            pos(2, 200.0, 200.0),
        ];
        assert_eq!(
            feed(&mut detector, &events),
            vec![BoundarySource::AllPlayersRepositioned]
        );
    }

    #[test]
    fn solo_reposition_is_a_boundary_unless_explained_by_a_self_kill() {
        let mut detector = MapChangeDetector::default();
        feed(&mut detector, &handshake(&[(1, 10.0, 10.0)]));
        let self_kill = [fin(1, (12.0, 9.0)), begin(1), pos(1, 400.0, 400.0)];
        assert!(feed(&mut detector, &self_kill).is_empty());
        // A shot that ended elsewhere does not explain the next reposition.
        let events = [fin(1, (900.0, 900.0)), pos(1, 600.0, 100.0)];
        assert_eq!(
            feed(&mut detector, &events),
            vec![BoundarySource::SoloReposition]
        );
    }

    #[test]
    fn a_departed_player_is_not_waited_for() {
        let mut detector = MapChangeDetector::default();
        feed(
            &mut detector,
            &handshake(&[(1, 10.0, 10.0), (2, 500.0, 500.0), (3, 900.0, 100.0)]),
        );
        detector.observe(&ServerEvent::PlayerLeave { player_id: 3 });
        let burst = [pos(1, 100.0, 200.0), pos(2, 700.0, 50.0)];
        assert_eq!(
            feed(&mut detector, &burst),
            vec![BoundarySource::AllPlayersRepositioned]
        );
    }

    #[test]
    fn board_mode_needs_unfinished_shots_and_a_quiet_period() {
        let mut board = BoardModeDetector::default();
        board.observe(&begin(1), 0);
        board.observe(&begin(2), 100);
        assert_eq!(
            board.poll(5_000),
            None,
            "two unfinished shots are not enough"
        );
        board.observe(&begin(1), 200);
        assert_eq!(board.poll(BOARD_QUIET_MS - 1), None);
        assert_eq!(board.poll(BOARD_QUIET_MS), Some(BoardChange::Entered));
        assert!(board.active());
        assert_eq!(board.poll(BOARD_QUIET_MS + 10), None);
        // Any finished shot means the game is simulating: resume.
        assert_eq!(
            board.observe(&fin(2, (0.0, 0.0)), 1_600),
            Some(BoardChange::Left)
        );
        assert!(!board.active());
        for t in [2_000, 2_010, 2_020] {
            board.observe(&begin(1), t);
        }
        assert_eq!(board.poll(4_000), Some(BoardChange::Entered));
        assert_eq!(board.new_map(), Some(BoardChange::Left));
        assert_eq!(board.poll(10_000), None);
    }

    #[test]
    fn vanished_debris_of_a_rehit_player_is_not_board_mode() {
        let mut board = BoardModeDetector::default();
        let kill = |board: &mut BoardModeDetector, t: i64| {
            board.observe(&fin(1, (500.0, 500.0)), t);
            for _ in 0..10 {
                board.observe(&begin(2), t);
            }
            board.observe(&pos(2, 100.0, 100.0), t);
        };
        kill(&mut board, 0);
        // Hit again: the new debris overwrites the old, which never finishes.
        kill(&mut board, 500);
        assert_eq!(board.poll(5_000), None);
        // Shots not followed by their player's respawn still count.
        board.observe(&fin(1, (500.0, 500.0)), 6_000);
        for _ in 0..3 {
            board.observe(&begin(2), 6_000);
        }
        board.observe(
            &ServerEvent::OwnEnergy {
                player_id: 1,
                energy: 5.0,
            },
            6_001,
        );
        assert_eq!(
            board.poll(6_000 + BOARD_QUIET_MS),
            Some(BoardChange::Entered)
        );
    }

    #[test]
    fn shots_of_a_departed_player_do_not_signal_board_mode() {
        let mut board = BoardModeDetector::default();
        for t in [0, 10, 20] {
            board.observe(&begin(7), t);
        }
        board.observe(&ServerEvent::PlayerLeave { player_id: 7 }, 30);
        assert_eq!(board.poll(5_000), None);
    }
}

/// Replays every recorded capture (`EINSTEINBOT_DB`, default `einsteinbot.sqlite3`) through
/// both detectors and prints how they compare with the disclosed `Planets` boundaries.
/// `cargo test --release --lib map_change::replay -- --ignored --nocapture`
#[cfg(test)]
mod replay {
    use super::*;
    use crate::protocol::RecordedEvent;

    #[test]
    #[ignore = "reads the local capture database"]
    fn detectors_on_recorded_captures() {
        let path =
            std::env::var("EINSTEINBOT_DB").unwrap_or_else(|_| "einsteinbot.sqlite3".to_owned());
        let db = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let captures: Vec<(i64, String)> = db
            .prepare("SELECT id, metadata_json FROM captures ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let (mut boundaries, mut found, mut false_boundaries) = (0, 0, 0);
        let (mut rounds, mut rounds_found, mut false_boards) = (0, 0, 0);
        let (mut delays, mut false_pause_ms) = (Vec::new(), 0);
        for (id, metadata) in captures {
            let metadata: serde_json::Value = serde_json::from_str(&metadata).unwrap();
            let canonical = metadata["client_name"]
                .as_str()
                .unwrap_or_default()
                .split(',')
                .next()
                .unwrap_or_default()
                .to_owned();
            let mut statement = db
                .prepare(
                    "SELECT timestamp_unix_ms, bot_name, event_json FROM events
                     WHERE capture_id=?1 AND direction='inbound' ORDER BY sequence",
                )
                .unwrap();
            let events: Vec<(i64, ServerEvent)> = statement
                .query_map([id], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .filter_map(|row| {
                    let (ts, bot, json) = row.unwrap();
                    // Other sessions' own events interleave arbitrarily; keep one connection.
                    if bot.is_some_and(|bot| bot != canonical) {
                        return None;
                    }
                    match serde_json::from_str(&json).unwrap() {
                        RecordedEvent::Inbound(event) => Some((ts, event)),
                        RecordedEvent::Outbound(_) => None,
                    }
                })
                .collect();
            let planets: Vec<i64> = events
                .iter()
                .filter(|(_, e)| matches!(e, ServerEvent::Planets { .. }))
                .map(|(ts, _)| *ts)
                .skip(1)
                .collect();
            let mut detector = MapChangeDetector::default();
            let detected: Vec<i64> = events
                .iter()
                .filter(|(_, e)| !matches!(e, ServerEvent::Planets { .. }))
                .filter_map(|(ts, e)| detector.observe(e).map(|_| *ts))
                .collect();
            boundaries += planets.len();
            found += planets
                .iter()
                .filter(|p| detected.iter().any(|d| (d - *p).abs() <= 200))
                .count();
            false_boundaries += detected
                .iter()
                .filter(|d| !planets.iter().any(|p| (*d - p).abs() <= 1_500))
                .count();

            // Board mode: poll every 100 ms between events, as the live loop polls.
            let mut board = BoardModeDetector::default();
            let mut entered_at = None;
            let mut last_fin = None;
            let mut previous = events.first().map_or(0, |(ts, _)| *ts);
            for (ts, event) in &events {
                let mut t = previous;
                while t <= *ts {
                    if board.poll(t) == Some(BoardChange::Entered) {
                        entered_at = Some(t);
                    }
                    t += 100;
                }
                previous = *ts;
                if matches!(event, ServerEvent::Planets { .. }) && planets.contains(ts) {
                    // A round end has a board phase: at least 4 s without a finished shot.
                    if last_fin.is_some_and(|fin| ts - fin >= 4_000) {
                        rounds += 1;
                        if let (Some(at), Some(fin)) = (entered_at, last_fin) {
                            rounds_found += 1;
                            delays.push(at - fin);
                        }
                    }
                    board.new_map();
                    entered_at = None;
                    continue;
                }
                if matches!(
                    event,
                    ServerEvent::ShotFin { .. } | ServerEvent::ShotFinished { .. }
                ) {
                    if let Some(at) = entered_at.take() {
                        false_boards += 1;
                        false_pause_ms += ts - at;
                        println!("capture {id}: false board mode for {} ms", ts - at);
                    }
                    last_fin = Some(*ts);
                }
                board.observe(event, *ts);
            }
            // A per-map capture (2026-11-27 on) ends at the boundary instead of containing it.
            let map = &metadata["map"];
            if map["ended_by"] == "boundary" {
                let end = map["ended_at_unix_ms"].as_i64().unwrap();
                let mut t = previous;
                while t <= end {
                    if board.poll(t) == Some(BoardChange::Entered) {
                        entered_at = Some(t);
                    }
                    t += 100;
                }
                if last_fin.is_some_and(|fin| end - fin >= 4_000) {
                    rounds += 1;
                    if let (Some(at), Some(fin)) = (entered_at, last_fin) {
                        rounds_found += 1;
                        delays.push(at - fin);
                    }
                }
            }
        }
        delays.sort();
        println!(
            "map boundaries: {found}/{boundaries} later Planets found without Planets, {false_boundaries} detections without a Planets"
        );
        println!(
            "board mode: {rounds_found}/{rounds} round ends detected; delay after the last finished shot (ms): median {:?} max {:?}; {false_boards} false detections pausing {} ms in total",
            delays.get(delays.len() / 2),
            delays.last(),
            false_pause_ms
        );
    }
}
