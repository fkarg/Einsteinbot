//! Per-game statistics printed when the client stops (P18). First what is in memory about each
//! recent map epoch (`MapRecord`, printed at once), then each recent map's estimate after a short
//! settle (`Settled`), scored against the disclosed planets when the server sent them
//! (`sendplanets 1`). Truth is used here only to score, never as estimator input.
use crate::estimator::{CountLimit, EstimatorSummary, LearnedCount, UpdateReport};
use crate::field_score::score_field;
use crate::map::PlanetMap;
use crate::protocol::{Planet, Point, ServerEvent};
use crate::simulation::{ShotObservation, WorldParameters};
use crate::strategy::MapBelief;
use std::collections::BTreeMap;
use std::fmt::Write;

/// A finished shot ending this close to another player hit it (player radius 4 plus slack, as
/// the estimator's contact classification uses).
const PLAYER_HIT_DISTANCE: f64 = 4.1;
/// Map epochs reported ("the last couple of games").
pub const RECENT_MAPS: usize = 3;

/// One session's own finished shots in one map epoch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ShotTally {
    pub shots: u32,
    pub hits: u32,
    /// Energy spent: the server deducts a shot's velocity from its player's energy.
    pub energy: f64,
}

/// What a session is doing now (shown by the live terminal view).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Activity {
    #[default]
    Connecting,
    /// Waiting for the next energy report (or passive).
    WaitingEnergy,
    /// Holding a shot until the live map has seen the queued paths (bounded wait).
    MapWait,
    /// Searching for a shot.
    Aiming,
    /// The end card: no commands are sent.
    BoardMode,
    /// The session has ended.
    Stopped,
}

impl Activity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::WaitingEnergy => "waiting for energy",
            Self::MapWait => "map wait",
            Self::Aiming => "aiming",
            Self::BoardMode => "board mode",
            Self::Stopped => "stopped",
        }
    }
}

/// One session's own shots per map epoch. Epochs are counted by the session's map boundaries
/// (`new_map`), as the live map counts the canonical session's, so the numbers agree with
/// `MapRecord::epoch`. Also the session's latest energy and activity, for the live view.
#[derive(Clone, Debug, Default)]
pub struct SessionTally {
    pub name: String,
    epoch: u64,
    pub games: BTreeMap<u64, ShotTally>,
    /// The latest reported energy.
    pub energy: Option<f64>,
    pub activity: Activity,
}

impl SessionTally {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// The shots of the current map epoch.
    pub fn current_game(&self) -> ShotTally {
        self.games.get(&self.epoch).cloned().unwrap_or_default()
    }

    /// A map boundary: later shots belong to the next epoch.
    pub fn new_map(&mut self) {
        self.epoch += 1;
    }

    /// Count `event`; `players` are the positions known before it.
    pub fn observe(
        &mut self,
        event: &ServerEvent,
        own_id: Option<u32>,
        players: &BTreeMap<u32, Point>,
    ) {
        match event {
            ServerEvent::ShotFin {
                player_id,
                velocity,
                points,
                ..
            } if Some(*player_id) == own_id => {
                let tally = self.games.entry(self.epoch).or_default();
                tally.shots += 1;
                tally.energy += velocity;
                let hit = points.last().is_some_and(|end| {
                    players.iter().any(|(id, p)| {
                        id != player_id
                            && f64::from(end.x - p.x).hypot(f64::from(end.y - p.y))
                                <= PLAYER_HIT_DISTANCE
                    })
                });
                tally.hits += u32::from(hit);
            }
            _ => {}
        }
    }
}

/// Publish lag of the paths of one map epoch: from a path's arrival at the live map to the
/// publication of the belief that has seen it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lag {
    pub paths: usize,
    pub last_ms: f64,
    pub max_ms: f64,
}

impl Lag {
    pub fn add(&mut self, ms: f64) {
        self.paths += 1;
        self.last_ms = ms;
        self.max_ms = self.max_ms.max(ms);
    }
}

/// What the live map holds about one map epoch.
#[derive(Clone, Debug, Default)]
pub struct MapRecord {
    pub epoch: u64,
    /// When the live map started recording this epoch.
    pub started: Option<std::time::Instant>,
    /// The capture recording this map, when recording.
    pub capture: Option<i64>,
    /// The disclosed planets (`Planets`), if the server sent them.
    pub truth: Option<Vec<Planet>>,
    pub belief: Option<MapBelief>,
    /// The update report of the update that published `belief`.
    pub report: Option<UpdateReport>,
    pub summary: Option<EstimatorSummary>,
    /// Paths queued for the estimator and not yet in `belief` (current epoch only).
    pub pending_paths: usize,
    pub lag: Lag,
}

/// One recent map's estimate after the stop-time settle.
#[derive(Clone, Debug)]
pub struct Settled {
    pub epoch: u64,
    pub belief: Option<MapBelief>,
    pub summary: EstimatorSummary,
    /// The epoch's completed paths (for the explored/unexplored split of the field error).
    pub observations: Vec<ShotObservation>,
    /// Wall time spent settling (whether work was left is `summary.search_pending`).
    pub settle_ms: f64,
}

/// The settled map against the disclosed planets.
#[derive(Clone, Debug, PartialEq)]
pub struct TruthScore {
    pub true_count: usize,
    pub found: usize,
    /// Disclosed planets with an estimated centre within 2 and 15 units.
    pub recall2: usize,
    pub recall15: usize,
    /// Estimated planets with no disclosed centre within 15 units.
    pub spurious: usize,
    /// Mean centre and radius error over the disclosed planets matched within 15 units (nearest
    /// estimated centre); `None` without matches.
    pub centre_error: Option<f64>,
    pub radius_error: Option<f64>,
    /// Gravity-field vector RMSE on the 20-unit grid in explored and unexplored cells
    /// (`field_score`); `None` when unavailable.
    pub field_explored: Option<f64>,
    pub field_unexplored: Option<f64>,
    /// The same RMSE for a flat (planet-free) map: the scale the field errors are judged on.
    pub flat_explored: Option<f64>,
    pub flat_unexplored: Option<f64>,
}

pub fn score(map: &PlanetMap, truth: &[Planet], observations: &[ShotObservation]) -> TruthScore {
    let centre = |a: &Planet, b: &Planet| (a.x - b.x).hypot(a.y - b.y);
    let nearest = |t: &Planet| {
        map.planets
            .iter()
            .min_by(|a, b| centre(a, t).total_cmp(&centre(b, t)))
    };
    let matched: Vec<(&Planet, &Planet)> = truth
        .iter()
        .filter_map(|t| nearest(t).map(|e| (t, e)))
        .filter(|(t, e)| centre(t, e) <= 15.0)
        .collect();
    let mean = |values: Vec<f64>| {
        (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
    };
    let field = score_field(
        &PlanetMap {
            planets: truth.to_vec(),
        },
        &MapBelief::new(map.clone(), None),
        observations,
        WorldParameters::default(),
    );
    let flat = score_field(
        &PlanetMap {
            planets: truth.to_vec(),
        },
        &MapBelief::new(PlanetMap { planets: vec![] }, None),
        observations,
        WorldParameters::default(),
    );
    let rmse = |stats: Option<&crate::field_score::FieldStats>| stats.and_then(|s| s.vector_rmse);
    TruthScore {
        true_count: truth.len(),
        found: map.planets.len(),
        recall2: matched.iter().filter(|(t, e)| centre(t, e) <= 2.0).count(),
        recall15: matched.len(),
        spurious: map
            .planets
            .iter()
            .filter(|e| !truth.iter().any(|t| centre(t, e) <= 15.0))
            .count(),
        centre_error: mean(matched.iter().map(|(t, e)| centre(t, e)).collect()),
        radius_error: mean(
            matched
                .iter()
                .map(|(t, e)| (t.radius - e.radius).abs())
                .collect(),
        ),
        field_explored: field.as_ref().and_then(|f| rmse(f.explored.as_ref())),
        field_unexplored: field.as_ref().and_then(|f| rmse(f.unexplored.as_ref())),
        flat_explored: flat.as_ref().and_then(|f| rmse(f.explored.as_ref())),
        flat_unexplored: flat.as_ref().and_then(|f| rmse(f.unexplored.as_ref())),
    }
}

fn optional(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.digits$}"))
}

/// A share as a whole percentage, `n/a` without a denominator.
fn percent(part: f64, whole: f64) -> String {
    if whole > 0.0 {
        format!("{:.0}%", 100.0 * part / whole)
    } else {
        "n/a".to_owned()
    }
}

/// ANSI styling for the stop report: on only when stderr is a terminal and `NO_COLOR` is unset.
#[derive(Clone, Copy)]
struct Style(bool);

impl Style {
    fn detect() -> Self {
        use std::io::IsTerminal;
        Style(std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none())
    }
    fn paint(self, code: &str, text: &str) -> String {
        if self.0 {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
    fn title(self, text: &str) -> String {
        self.paint("1;36", text)
    }
    fn label(self, text: &str) -> String {
        self.paint("2", &format!("{text:<9}"))
    }
    fn good(self, text: &str) -> String {
        self.paint("32", text)
    }
    fn warn(self, text: &str) -> String {
        self.paint("1;33", text)
    }
}

/// One labelled, indented report line.
fn line(out: &mut String, style: Style, label: &str, text: &str) {
    let _ = writeln!(out, "  {} {text}", style.label(label));
}

/// Median and maximum per-planet centre sd (square root of the x and y variances summed).
fn centre_sds(belief: &MapBelief) -> Option<(f64, f64)> {
    let covariance = belief.parameter_covariance.as_ref()?;
    let mut sds: Vec<f64> = (0..belief.map.planets.len())
        .map(|i| (covariance[(3 * i, 3 * i)] + covariance[(3 * i + 1, 3 * i + 1)]).sqrt())
        .collect();
    sds.sort_by(f64::total_cmp);
    Some((*sds.get(sds.len() / 2)?, *sds.last()?))
}

fn belief_lines(out: &mut String, style: Style, belief: &MapBelief) {
    let covariance = match centre_sds(belief) {
        Some((median, max)) => format!("centre sd median {median:.2}, max {max:.2}"),
        None => format!(
            "covariance n/a ({})",
            belief
                .covariance_unavailable_reason
                .map_or("no reason".to_owned(), |r| format!("{r:?}"))
        ),
    };
    let correction = match (
        &belief.residual_correction,
        belief.residual_correction_unavailable_reason,
    ) {
        (Some(c), _) => format!("correction on (max {:.1e})", c.max_magnitude),
        (None, reason) => format!(
            "correction off ({})",
            reason.map_or("no reason".to_owned(), |r| format!("{r:?}"))
        ),
    };
    line(
        out,
        style,
        "certainty",
        &format!("{covariance} · {correction}"),
    );
    if let Some(text) = belief.count_limit.as_ref().and_then(count_limit_line) {
        let text = if belief
            .count_limit
            .as_ref()
            .is_some_and(|l| l.detected_at_path.is_some())
        {
            style.warn(&text)
        } else {
            text
        };
        line(out, style, "count", &text);
    }
}

/// The count-limit warning (P23) in words; `None` below the cap (nothing to say).
pub fn count_limit_line(limit: &CountLimit) -> Option<String> {
    match (limit.at_cap, limit.detected_at_path) {
        (_, Some(path)) => Some(format!(
            "WARNING at cap {} with planet-sized residuals since path {path}: maybe more planets (larger --planets)",
            limit.cap
        )),
        (true, None) => Some(format!("at cap {}; no sign of more planets", limit.cap)),
        (false, None) => None,
    }
}

/// The search state: pending or not, with the limit the last update hit.
pub fn search_state(summary: &EstimatorSummary) -> String {
    match (summary.search_pending, summary.search_limit) {
        (true, Some(limit)) => format!("pending ({limit:?})"),
        (true, None) => "pending".to_owned(),
        (false, Some(limit)) => format!("idle ({limit:?})"),
        (false, None) => "settled".to_owned(),
    }
}

fn summary_lines(out: &mut String, style: Style, summary: &EstimatorSummary) {
    let count = summary
        .model
        .total_planets
        .map_or("from prior".to_owned(), |n| format!("fixed {n}"));
    line(
        out,
        style,
        "map",
        &format!(
            "{} planets · {} of {} paths searched · search {}",
            summary.planets,
            summary.searched,
            summary.paths,
            search_state(summary)
        ),
    );
    line(
        out,
        style,
        "model",
        &format!(
            "unseen {:.1} of ≈{:.0} ({count}) · noise s {:.3} l {:.0} white {:.0e} · ρ0 {}",
            summary.unseen,
            summary.expected_total,
            summary.model.scale * 100.0,
            summary.model.length,
            summary.model.white,
            optional(summary.rho0, 0),
        ),
    );
    if let Some(learned) = &summary.learned {
        line(out, style, "count", &learned_line(learned));
    }
}

/// The dynamic planet count (P25, `--learn-planets`) in words: the count in use and its latest
/// changes.
pub fn learned_line(learned: &LearnedCount) -> String {
    let changes: Vec<String> = learned
        .changes
        .iter()
        .rev()
        .take(3)
        .rev()
        .map(|c| format!("{}→{} map {} path {}", c.from, c.to, c.map, c.at_path))
        .collect();
    let mut text = format!("learned {} (start {})", learned.planets, learned.start);
    if !changes.is_empty() {
        let skipped = learned.changes.len() - changes.len();
        let _ = write!(
            text,
            ": {}{}",
            if skipped > 0 { "… " } else { "" },
            changes.join(", ")
        );
    }
    text
}

fn tally_lines(out: &mut String, style: Style, epoch: u64, sessions: &[SessionTally]) {
    for session in sessions {
        let Some(tally) = session.games.get(&epoch) else {
            continue;
        };
        let rate = percent(f64::from(tally.hits), f64::from(tally.shots));
        let per_energy = optional(
            (tally.energy > 0.0).then(|| 100.0 * f64::from(tally.hits) / tally.energy),
            1,
        );
        line(
            out,
            style,
            &session.name,
            &format!(
                "{} hit rate ({}/{}) · {} hits per 100 energy",
                style.good(&rate),
                tally.hits,
                tally.shots,
                per_energy
            ),
        );
    }
}

/// The epochs worth reporting: the last `RECENT_MAPS` that have a map record or shots.
pub fn recent_epochs(maps: &[MapRecord], sessions: &[SessionTally]) -> Vec<u64> {
    let mut epochs: Vec<u64> = maps
        .iter()
        .map(|m| m.epoch)
        .chain(sessions.iter().flat_map(|s| s.games.keys().copied()))
        .collect();
    epochs.sort_unstable();
    epochs.dedup();
    let skip = epochs.len().saturating_sub(RECENT_MAPS);
    epochs.split_off(skip)
}

/// Everything in memory about the recent map epochs, printed before any settling.
pub fn immediate_report(maps: &[MapRecord], sessions: &[SessionTally]) -> String {
    let style = Style::detect();
    let mut out = String::new();
    for epoch in recent_epochs(maps, sessions) {
        let map = maps.iter().find(|m| m.epoch == epoch);
        let mut title = format!("━ map {epoch} · now");
        if let Some(capture) = map.and_then(|m| m.capture) {
            let _ = write!(title, " · capture {capture}");
        }
        if let Some(truth) = map.and_then(|m| m.truth.as_ref()) {
            let _ = write!(title, " · {} planets disclosed", truth.len());
        }
        let _ = writeln!(out, "{}", style.title(&title));
        tally_lines(&mut out, style, epoch, sessions);
        let Some(map) = map else { continue };
        if let Some(summary) = &map.summary {
            summary_lines(&mut out, style, summary);
        }
        match &map.belief {
            Some(belief) => belief_lines(&mut out, style, belief),
            None => line(&mut out, style, "certainty", "no map published yet"),
        }
        line(
            &mut out,
            style,
            "lag",
            &format!(
                "{} queued · last {:.0} ms · max {:.0} ms over {} paths",
                map.pending_paths, map.lag.last_ms, map.lag.max_ms, map.lag.paths
            ),
        );
        if let Some(report) = &map.report {
            let total: f64 = report.parts.iter().map(|p| p.total_ms).sum();
            let mut parts: Vec<_> = report.parts.iter().collect();
            parts.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
            let top: Vec<String> = parts
                .iter()
                .take(3)
                .map(|p| format!("{} {}", p.part.name(), percent(p.total_ms, total)))
                .collect();
            let violations = if report.violations.is_empty() {
                String::new()
            } else {
                style.warn(&format!(" · {} ceiling hits", report.violations.len()))
            };
            line(
                &mut out,
                style,
                "update",
                &format!(
                    "{} evals · {}{violations}",
                    report.evaluations,
                    top.join(" · ")
                ),
            );
        }
    }
    out
}

/// One map's settled estimate, scored against `truth` when the planets were disclosed.
pub fn settled_report(settled: &Settled, truth: Option<&[Planet]>) -> String {
    let style = Style::detect();
    let mut out = String::new();
    let pending = if settled.summary.search_pending {
        " · work still pending"
    } else {
        ""
    };
    let _ = writeln!(
        out,
        "{}",
        style.title(&format!(
            "━ map {} · settled in {:.0} ms{pending}",
            settled.epoch, settled.settle_ms
        ))
    );
    summary_lines(&mut out, style, &settled.summary);
    let disclosed = truth.map_or(String::new(), |t| format!(" · disclosed {}", t.len()));
    line(
        &mut out,
        style,
        "count",
        &format!(
            "{} found · likely {} in total{disclosed}",
            settled.summary.planets, settled.summary.likely_total
        ),
    );
    if let Some(belief) = &settled.belief {
        belief_lines(&mut out, style, belief);
        if let Some(truth) = truth {
            let s = score(&belief.map, truth, &settled.observations);
            let n = s.true_count as f64;
            line(
                &mut out,
                style,
                "vs truth",
                &format!(
                    "found {} within 2 · {} within 15 · {} spurious",
                    style.good(&percent(s.recall2 as f64, n)),
                    percent(s.recall15 as f64, n),
                    s.spurious
                ),
            );
            line(
                &mut out,
                style,
                "",
                &format!(
                    "centre error {} · radius error {}",
                    optional(s.centre_error, 2),
                    optional(s.radius_error, 2)
                ),
            );
            let of_flat = |error: Option<f64>, flat: Option<f64>| match (error, flat) {
                (Some(e), Some(f)) if f > 0.0 => format!("{:.2}%", 100.0 * e / f),
                _ => "n/a".to_owned(),
            };
            line(
                &mut out,
                style,
                "",
                &format!(
                    "field error vs flat: explored {} · unexplored {}",
                    style.good(&of_flat(s.field_explored, s.flat_explored)),
                    of_flat(s.field_unexplored, s.flat_unexplored)
                ),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::estimator::{Estimator, Settings};

    fn planet(x: f64, y: f64, radius: f64) -> Planet {
        Planet {
            x,
            y,
            radius,
            mass: radius.powi(3) / 10.0,
        }
    }

    #[test]
    fn session_tally_counts_own_shots_hits_and_energy_per_map_epoch() {
        let mut tally = SessionTally::new("Curie");
        let players = BTreeMap::from([
            (1, Point { x: 0.0, y: 0.0 }),
            (2, Point { x: 100.0, y: 0.0 }),
        ]);
        let shot = |player_id, end: f32| ServerEvent::ShotFin {
            player_id,
            angle: 0.0,
            velocity: 12.0,
            points: vec![Point { x: 0.0, y: 0.0 }, Point { x: end, y: 0.0 }],
        };
        tally.observe(&shot(1, 50.0), Some(1), &players);
        tally.new_map();
        tally.observe(&shot(1, 98.0), Some(1), &players);
        tally.observe(&shot(2, 0.0), Some(1), &players);
        assert_eq!(
            tally.games[&0],
            ShotTally {
                shots: 1,
                hits: 0,
                energy: 12.0
            }
        );
        assert_eq!(
            tally.games[&1],
            ShotTally {
                shots: 1,
                hits: 1,
                energy: 12.0
            }
        );
    }

    #[test]
    fn truth_score_matches_nearest_centres() {
        let truth = vec![
            planet(300.0, 300.0, 30.0),
            planet(800.0, 500.0, 25.0),
            planet(1400.0, 700.0, 20.0),
        ];
        let map = PlanetMap {
            planets: vec![
                planet(301.0, 300.0, 31.0),
                planet(810.0, 500.0, 25.0),
                planet(100.0, 900.0, 20.0),
            ],
        };
        let s = score(&map, &truth, &[]);
        assert_eq!((s.recall2, s.recall15, s.spurious), (1, 2, 1));
        assert_eq!(s.centre_error, Some(5.5));
        assert_eq!(s.radius_error, Some(0.5));
        assert_eq!(s.field_explored, None, "no paths: nothing explored");
        assert!(s.field_unexplored.is_some_and(f64::is_finite));
    }

    #[test]
    fn reports_cover_the_recent_maps_in_memory_and_after_settling() {
        let estimator = Estimator::new(Settings {
            planets: Some(22),
            ..Settings::default()
        });
        let summary = estimator.summary();
        assert_eq!(summary.expected_total, 22.0);
        let truth = vec![planet(300.0, 300.0, 30.0)];
        let maps: Vec<MapRecord> = (0..5)
            .map(|epoch| MapRecord {
                epoch,
                truth: Some(truth.clone()),
                belief: Some(MapBelief::new(PlanetMap { planets: vec![] }, None)),
                summary: Some(summary.clone()),
                ..MapRecord::default()
            })
            .collect();
        let mut session = SessionTally::new("Curie");
        session.games.insert(
            4,
            ShotTally {
                shots: 3,
                hits: 1,
                energy: 50.0,
            },
        );
        let text = immediate_report(&maps, &[session]);
        assert!(!text.contains("map 1 "), "only the last {RECENT_MAPS} maps");
        assert!(text.contains("━ map 2 · now · 1 planets disclosed"));
        assert!(text.contains("33% hit rate (1/3) · 2.0 hits per 100 energy"));
        assert!(text.contains("(fixed 22)"));
        assert!(text.contains("covariance n/a (NotComputed)"));
        let settled = Settled {
            epoch: 4,
            belief: maps[4].belief.clone(),
            summary,
            observations: vec![],
            settle_ms: 1000.0,
        };
        let text = settled_report(&settled, Some(&truth));
        assert!(text.contains("0 found · likely 22 in total · disclosed 1"));
        assert!(text.contains("found 0% within 2 · 0% within 15 · 0 spurious"));
    }

    #[test]
    fn the_count_limit_warning_is_reported_at_the_cap_only() {
        let mut limit = CountLimit {
            cap: 24,
            at_cap: false,
            detected_at_path: None,
            recent_magnitudes: vec![],
        };
        assert_eq!(count_limit_line(&limit), None);
        limit.at_cap = true;
        assert_eq!(
            count_limit_line(&limit).as_deref(),
            Some("at cap 24; no sign of more planets")
        );
        limit.detected_at_path = Some(57);
        let mut belief = MapBelief::new(PlanetMap { planets: vec![] }, None);
        belief.count_limit = Some(limit);
        let mut out = String::new();
        belief_lines(&mut out, Style(false), &belief);
        assert!(out.contains("WARNING at cap 24 with planet-sized residuals since path 57"));
    }

    #[test]
    fn the_learned_count_is_reported_with_its_latest_changes() {
        let mut summary = Estimator::new(Settings {
            learn_planets: true,
            ..Settings::default()
        })
        .summary();
        let mut out = String::new();
        summary_lines(&mut out, Style(false), &summary);
        assert!(out.contains("learned 24 (start 24)\n"), "{out}");
        let learned = summary.learned.as_mut().unwrap();
        for (from, to) in [(24, 25), (25, 26), (26, 27), (27, 28)] {
            learned.changes.push(crate::estimator::CountChange {
                from,
                to,
                map: 1,
                at_path: 40,
            });
        }
        learned.planets = 28;
        assert_eq!(
            learned_line(learned),
            "learned 28 (start 24): … 25→26 map 1 path 40, 26→27 map 1 path 40, 27→28 map 1 path 40"
        );
        // Without the dynamic mode there is no count line.
        let mut out = String::new();
        summary_lines(&mut out, Style(false), &Estimator::default().summary());
        assert!(!out.contains("learned"));
    }
}
