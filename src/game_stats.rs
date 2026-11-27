//! Per-game statistics printed when the client stops (P18). First what is in memory about each
//! recent map epoch (`MapRecord`, printed at once), then each recent map's estimate after a short
//! settle (`Settled`), scored against the disclosed planets when the server sent them
//! (`sendplanets 1`). Truth is used here only to score, never as estimator input.
use crate::estimator::{EstimatorSummary, UpdateReport};
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

/// One session's own shots per map epoch. Epochs are counted by `Planets` events, as the live
/// map counts them, so the numbers agree with `MapRecord::epoch`.
#[derive(Clone, Debug, Default)]
pub struct SessionTally {
    pub name: String,
    epoch: u64,
    pub games: BTreeMap<u64, ShotTally>,
}

impl SessionTally {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }

    /// Count `event`; `players` are the positions known before it.
    pub fn observe(
        &mut self,
        event: &ServerEvent,
        own_id: Option<u32>,
        players: &BTreeMap<u32, Point>,
    ) {
        match event {
            ServerEvent::Planets { .. } => self.epoch += 1,
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
    }
}

fn optional(value: Option<f64>, digits: usize) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |v| format!("{v:.digits$}"))
}

/// Per-planet centre sd (square root of the x and y variances summed), from the covariance.
fn centre_sds(belief: &MapBelief) -> Option<Vec<f64>> {
    let covariance = belief.parameter_covariance.as_ref()?;
    Some(
        (0..belief.map.planets.len())
            .map(|i| (covariance[(3 * i, 3 * i)] + covariance[(3 * i + 1, 3 * i + 1)]).sqrt())
            .collect(),
    )
}

fn belief_lines(out: &mut String, belief: &MapBelief) {
    match centre_sds(belief) {
        Some(sds) => {
            let list: Vec<String> = sds.iter().map(|sd| format!("{sd:.2}")).collect();
            let _ = writeln!(
                out,
                "    covariance: available; centre sd [{}]",
                list.join(" ")
            );
        }
        None => {
            let _ = writeln!(
                out,
                "    covariance: unavailable ({})",
                belief
                    .covariance_unavailable_reason
                    .map_or("no reason recorded".to_owned(), |r| format!("{r:?}"))
            );
        }
    }
    match (
        &belief.residual_correction,
        belief.residual_correction_unavailable_reason,
    ) {
        (Some(correction), _) => {
            let _ = writeln!(
                out,
                "    residual correction: applied (max {:.2e}, {} rows, length {:.0})",
                correction.max_magnitude, correction.rows, correction.length
            );
        }
        (None, reason) => {
            let _ = writeln!(
                out,
                "    residual correction: not applied ({})",
                reason.map_or("no reason recorded".to_owned(), |r| format!("{r:?}"))
            );
        }
    }
}

fn summary_line(out: &mut String, summary: &EstimatorSummary) {
    let _ = writeln!(
        out,
        "    estimator: K={} paths={} searched={} search={} model s*100={:.3} l={:.0} white={:.1e} N_u={:.2} rho0={} expected total={:.1} ({})",
        summary.planets,
        summary.paths,
        summary.searched,
        match (summary.search_pending, summary.search_limit) {
            (true, Some(limit)) => format!("pending ({limit:?})"),
            (true, None) => "pending".to_owned(),
            (false, Some(limit)) => format!("idle ({limit:?})"),
            (false, None) => "settled".to_owned(),
        },
        summary.model.scale * 100.0,
        summary.model.length,
        summary.model.white,
        summary.unseen,
        optional(summary.rho0, 0),
        summary.expected_total,
        summary
            .model
            .total_planets
            .map_or("count from the prior".to_owned(), |n| format!(
                "count fixed at {n}"
            ))
    );
}

fn tally_line(out: &mut String, epoch: u64, sessions: &[SessionTally]) {
    for session in sessions {
        let Some(tally) = session.games.get(&epoch) else {
            continue;
        };
        let _ = writeln!(
            out,
            "    {}: shots={} hits={} energy={:.0} hits/100 energy={}",
            session.name,
            tally.shots,
            tally.hits,
            tally.energy,
            optional(
                (tally.energy > 0.0).then(|| 100.0 * f64::from(tally.hits) / tally.energy),
                2
            )
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
    let mut out = String::new();
    for epoch in recent_epochs(maps, sessions) {
        let map = maps.iter().find(|m| m.epoch == epoch);
        let _ = writeln!(
            out,
            "map {epoch} (in memory now){}",
            map.and_then(|m| m.truth.as_ref())
                .map_or(String::new(), |t| format!(
                    ": {} planets disclosed",
                    t.len()
                ))
        );
        tally_line(&mut out, epoch, sessions);
        let Some(map) = map else { continue };
        if let Some(summary) = &map.summary {
            summary_line(&mut out, summary);
        }
        match &map.belief {
            Some(belief) => {
                let _ = writeln!(
                    out,
                    "    published map: {} planets, search limit {}",
                    belief.map.planets.len(),
                    belief
                        .search_limit
                        .map_or("none".to_owned(), |l| format!("{l:?}"))
                );
                belief_lines(&mut out, belief);
            }
            None => {
                let _ = writeln!(out, "    published map: none yet");
            }
        }
        let _ = writeln!(
            out,
            "    map lag: {} paths queued; publish lag last {:.0} ms, max {:.0} ms over {} paths",
            map.pending_paths, map.lag.last_ms, map.lag.max_ms, map.lag.paths
        );
        if let Some(report) = &map.report {
            let mut parts: Vec<_> = report.parts.iter().collect();
            parts.sort_by(|a, b| b.total_ms.total_cmp(&a.total_ms));
            let top: Vec<String> = parts
                .iter()
                .take(4)
                .map(|p| format!("{} {:.0} ms", p.part.name(), p.total_ms))
                .collect();
            let _ = writeln!(
                out,
                "    last update: {} evaluations; {}{}",
                report.evaluations,
                top.join(", "),
                if report.violations.is_empty() {
                    String::new()
                } else {
                    format!("; {} ceiling violations", report.violations.len())
                }
            );
        }
    }
    out
}

/// One map's settled estimate, scored against `truth` when the planets were disclosed.
pub fn settled_report(settled: &Settled, truth: Option<&[Planet]>) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "map {} settled ({:.0} ms{}):",
        settled.epoch,
        settled.settle_ms,
        if settled.summary.search_pending {
            ", work still pending"
        } else {
            ""
        }
    );
    summary_line(&mut out, &settled.summary);
    let _ = writeln!(
        out,
        "    planet count: {} found, expected total {:.1}{}",
        settled.summary.planets,
        settled.summary.expected_total,
        truth.map_or(String::new(), |t| format!(", disclosed {}", t.len()))
    );
    if let Some(belief) = &settled.belief {
        belief_lines(&mut out, belief);
        if let Some(truth) = truth {
            let s = score(&belief.map, truth, &settled.observations);
            let _ = writeln!(
                out,
                "    vs disclosed: recall {}/{} within 2, {}/{} within 15, spurious {}, centre error {}, radius error {}, field RMSE explored {} unexplored {}",
                s.recall2,
                s.true_count,
                s.recall15,
                s.true_count,
                s.spurious,
                optional(s.centre_error, 2),
                optional(s.radius_error, 2),
                s.field_explored
                    .map_or("unavailable".to_owned(), |v| format!("{v:.2e}")),
                s.field_unexplored
                    .map_or("unavailable".to_owned(), |v| format!("{v:.2e}")),
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
        tally.observe(&ServerEvent::Planets { planets: vec![] }, Some(1), &players);
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
        assert!(text.contains("map 2 (in memory now): 1 planets disclosed"));
        assert!(text.contains("Curie: shots=3 hits=1 energy=50 hits/100 energy=2.00"));
        assert!(text.contains("count fixed at 22"));
        assert!(text.contains("covariance: unavailable (NotComputed)"));
        let settled = Settled {
            epoch: 4,
            belief: maps[4].belief.clone(),
            summary,
            observations: vec![],
            settle_ms: 1000.0,
        };
        let text = settled_report(&settled, Some(&truth));
        assert!(text.contains("planet count: 0 found, expected total 22.0, disclosed 1"));
        assert!(text.contains("recall 0/1 within 2, 0/1 within 15, spurious 0"));
    }
}
