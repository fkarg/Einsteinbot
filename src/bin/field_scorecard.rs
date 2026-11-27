//! Field evaluation of the physical-planet map from an ordered capture; truth is used only for scoring.
use einsteinbot::estimator::{Estimator, Part, Settings};
use einsteinbot::event_store::EventLog;
use einsteinbot::field_score::score_field;
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

struct Replay {
    estimator: Estimator,
    /// Number of updates in which a wall-clock part ceiling was exceeded.
    ceiling_violations: usize,
    fit_micros: u128,
    /// Duration of the most recent path update.
    update_micros: u128,
    observations: Vec<ShotObservation>,
    truth: Option<PlanetMap>,
    epoch: usize,
    completed_total: usize,
    empty_is_flat: bool,
}

impl Replay {
    fn new(empty_is_flat: bool, settings: Settings) -> Self {
        let mut estimator = Estimator::default();
        estimator.settings = settings;
        Self {
            ceiling_violations: 0,
            estimator,
            fit_micros: 0,
            update_micros: 0,
            observations: Vec::new(),
            truth: None,
            epoch: 0,
            completed_total: 0,
            empty_is_flat,
        }
    }

    fn ingest(&mut self, sequence: i64, event: &ServerEvent) -> Option<Value> {
        match event {
            ServerEvent::Planets { planets } => {
                self.epoch += 1;
                self.truth = (!planets.is_empty() || self.empty_is_flat).then(|| PlanetMap {
                    planets: planets.clone(),
                });
                self.observations.clear();
                self.estimator.reset();
                self.fit_micros = 0;
                None
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => {
                let mut row = self.snapshot(sequence, "before_launch");
                row["shot"] = json!({"player_id": player_id, "angle": angle, "velocity": velocity});
                Some(row)
            }
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                ..
            } => {
                let shot = ShotObservation::try_from(event).ok()?;
                if self.observations.contains(&shot) {
                    return None;
                }
                let start = Instant::now();
                let never = AtomicBool::new(false);
                self.estimator.observe(event, &never);
                self.estimator.settle(&never);
                if self.estimator.settings.audit_enabled {
                    self.estimator.finish_audit();
                    self.estimator.settle(&never);
                }
                self.update_micros = start.elapsed().as_micros();
                self.fit_micros += self.update_micros;
                if let Some(report) = self.estimator.last_report() {
                    if !report.violations.is_empty() {
                        self.ceiling_violations += 1;
                        eprintln!(
                            "PART CEILING EXCEEDED at sequence {sequence}: {}",
                            report
                                .violations
                                .iter()
                                .map(|v| format!(
                                    "{} {:.1} ms > {:.1} ms",
                                    v.part.name(),
                                    v.observed_ms,
                                    v.ceiling_ms
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        );
                    }
                }
                self.observations.push(shot);
                self.completed_total += 1;
                let mut row = self.snapshot(sequence, "after_completion");
                row["shot"] = json!({"player_id": player_id, "angle": angle, "velocity": velocity});
                Some(row)
            }
            ServerEvent::PlayerPosition { .. } | ServerEvent::PlayerLeave { .. } => {
                self.estimator.observe(event, &AtomicBool::new(false));
                None
            }
            _ => None,
        }
    }

    fn snapshot(&self, sequence: i64, phase: &str) -> Value {
        let empty = MapBelief::new(PlanetMap { planets: vec![] }, None);
        let belief = self.estimator.belief().unwrap_or(&empty);
        let world = WorldParameters::default();
        let mut row = json!({ "sequence": sequence, "epoch": self.epoch, "phase": phase,
            "completed_paths": self.observations.len(),
            "completed_total": self.completed_total,
            "truth_status": if self.truth.is_some() { "disclosed" } else { "unavailable" },
            "fit_ms": self.fit_micros as f64 / 1000.0,
            "update_ms": self.update_micros as f64 / 1000.0,
            "search_limit": belief.search_limit,
            "update_report": self.estimator.last_report(),
            "belief_published": self.estimator.belief().is_some(),
            "physical_planets": belief.map.planets.len(),
            "physical": null, "flat": null, "oracle": null });
        if let Some(truth) = &self.truth {
            row["physical"] = json!(score_field(truth, belief, &self.observations, world));
            row["flat"] = json!(score_field(truth, &empty, &self.observations, world));
            row["oracle"] = json!(score_field(
                truth,
                &MapBelief::new(truth.clone(), None),
                &self.observations,
                world
            ));
        }
        row
    }
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let mut db = String::from("learning-scenarios.sqlite3");
    let mut capture: Option<i64> = None;
    let mut empty_is_flat = false;
    let mut max_completed = None;
    let mut settings = Settings::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db-path" => db = args.next().ok_or("--db-path requires a value")?,
            "--capture-id" => {
                capture = Some(
                    args.next()
                        .ok_or("--capture-id requires a value")?
                        .parse()?,
                )
            }
            "--empty-is-flat" => empty_is_flat = true,
            "--max-completed" => {
                max_completed = Some(
                    args.next()
                        .ok_or("--max-completed requires a value")?
                        .parse::<usize>()?,
                )
            }
            "--max-planets" => {
                settings.max_planets = args
                    .next()
                    .ok_or("--max-planets requires a value")?
                    .parse()?
            }
            "--slice-evaluations" => {
                settings.slice_evaluations = args
                    .next()
                    .ok_or("--slice-evaluations requires a value")?
                    .parse()?
            }
            "--slice-ceiling-ms" => {
                let ms: u64 = args
                    .next()
                    .ok_or("--slice-ceiling-ms requires a value")?
                    .parse()?;
                settings.slice_ceiling = (ms > 0).then(|| Duration::from_millis(ms));
            }
            "--slices-per-path" => {
                settings.slices_per_path = args
                    .next()
                    .ok_or("--slices-per-path requires a value")?
                    .parse()?
            }
            "--threads" => {
                settings.threads = args.next().ok_or("--threads requires a value")?.parse()?
            }
            "--audit" => settings.audit_enabled = true,
            "--ceiling-ms" => {
                let value = args.next().ok_or("--ceiling-ms requires PART=MS")?;
                let (name, ms) = value.split_once('=').ok_or("--ceiling-ms needs PART=MS")?;
                let part = Part::from_name(name).ok_or_else(|| {
                    format!(
                        "unknown part {name}; parts: {}",
                        Part::ALL.map(Part::name).join(", ")
                    )
                })?;
                settings
                    .ceilings
                    .0
                    .insert(part, Duration::from_secs_f64(ms.parse::<f64>()? / 1e3));
            }
            "--help" | "-h" => {
                println!(
                    "Usage: field_scorecard --capture-id ID [--db-path PATH] [--empty-is-flat] [--max-completed N] [--max-planets N] [--slice-evaluations N] [--slice-ceiling-ms MS] [--slices-per-path N] [--threads N] [--audit] [--ceiling-ms PART=MS]...\nThe estimator keeps its map incrementally: each path gets a local refit, then --slices-per-path search slices of --slice-evaluations evaluations (a deterministic work limit; results do not depend on load unless --slice-ceiling-ms, the wall-clock ceiling per slice, 0 for none, cuts a slice). --audit runs the from-scratch audit to completion after every path that is due (slow). --ceiling-ms fails the run (exit 2, naming the part) when a part of an update exceeds the wall-clock ceiling.\nJSON lines report causal pre-launch and post-completion physical-map field scores. Empty planet messages are unknown unless --empty-is-flat is explicitly set."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let capture = capture.ok_or("--capture-id is required")?;
    if !std::path::Path::new(&db).is_file() {
        return Err(format!("capture database not found: {db}").into());
    }
    if max_completed == Some(0) {
        return Err("--max-completed must be positive".into());
    }
    let events = EventLog::read_capture(&db, capture)?;
    if events.is_empty() {
        return Err(format!("capture {capture} is missing or has no recorded events").into());
    }
    let mut replay = Replay::new(empty_is_flat, settings);
    println!(
        "{}",
        json!({"type":"field_scorecard", "capture_id":capture, "db_path":db,
        "grid_cell_size":einsteinbot::coverage::CELL_SIZE, "world":"NewtonWars default dimensions",
        "build_revision":option_env!("EINSTEINBOT_GIT_REVISION"), "source_changes":option_env!("EINSTEINBOT_GIT_DIRTY").and_then(|value| value.parse::<bool>().ok())})
    );
    for row in events {
        if let RecordedEvent::Inbound(event) = row.event {
            if let Some(score) = replay.ingest(row.sequence, &event) {
                println!("{}", score);
                if max_completed.is_some_and(|limit| replay.completed_total >= limit) {
                    break;
                }
            }
        }
    }
    if replay.ceiling_violations > 0 {
        eprintln!(
            "{} update(s) exceeded a part ceiling",
            replay.ceiling_violations
        );
        std::process::exit(2);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use einsteinbot::protocol::Point;

    fn begin(angle: f64) -> ServerEvent {
        ServerEvent::ShotBegin {
            player_id: 1,
            angle,
            velocity: 8.0,
        }
    }

    fn fin() -> ServerEvent {
        ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 8.0,
            points: vec![
                Point { x: 1.0, y: 1.0 },
                Point { x: 41.0, y: 1.0 },
                Point { x: 41.0, y: 1.0 },
            ],
        }
    }

    #[test]
    fn launch_snapshot_excludes_shots_that_finish_after_launch_and_resets_per_epoch() {
        let mut replay = Replay::new(true, Settings::default());
        replay.ingest(1, &ServerEvent::Planets { planets: vec![] });
        let initial = replay.ingest(2, &begin(0.0)).unwrap();
        assert_eq!(initial["completed_paths"], 0);
        assert!(initial["physical"]["explored"].is_null());
        replay.ingest(3, &fin());
        let later = replay.ingest(4, &begin(45.0)).unwrap();
        assert_eq!(later["completed_paths"], 1);
        assert!(later["physical"]["explored"]["samples"].as_u64().unwrap() > 0);
        replay.ingest(5, &ServerEvent::Planets { planets: vec![] });
        let reset = replay.ingest(6, &begin(0.0)).unwrap();
        assert_eq!(reset["completed_paths"], 0);
        assert_eq!(reset["completed_total"], 1);
        assert_eq!(reset["epoch"], 2);
        assert!(reset["physical"]["explored"].is_null());
        let after_reset = replay.ingest(7, &fin()).unwrap();
        assert_eq!(after_reset["completed_paths"], 1);
        assert_eq!(after_reset["completed_total"], 2);
    }

    #[test]
    fn empty_planets_are_not_implicitly_a_disclosed_flat_world() {
        let mut replay = Replay::new(false, Settings::default());
        replay.ingest(1, &ServerEvent::Planets { planets: vec![] });
        let row = replay.ingest(2, &begin(0.0)).unwrap();
        assert_eq!(row["truth_status"], "unavailable");
        assert!(row["physical"].is_null());
    }
}
