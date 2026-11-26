//! Field evaluation of the physical-planet map from an ordered capture; truth is used only for scoring.
use einsteinbot::estimator::Estimator;
use einsteinbot::event_store::EventLog;
use einsteinbot::field_score::score_field;
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

struct Replay {
    estimator: Estimator,
    fit_micros: u128,
    observations: Vec<ShotObservation>,
    truth: Option<PlanetMap>,
    epoch: usize,
    completed_total: usize,
    empty_is_flat: bool,
}

impl Replay {
    fn new(empty_is_flat: bool) -> Self {
        Self {
            estimator: Estimator::default(),
            fit_micros: 0,
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
                self.estimator.observe(event, &AtomicBool::new(false));
                self.fit_micros += start.elapsed().as_micros();
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
            "--help" | "-h" => {
                println!(
                    "Usage: field_scorecard --capture-id ID [--db-path PATH] [--empty-is-flat] [--max-completed N]\nJSON lines report causal pre-launch and post-completion physical-map field scores. Empty planet messages are unknown unless --empty-is-flat is explicitly set."
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
    let mut replay = Replay::new(empty_is_flat);
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
        let mut replay = Replay::new(true);
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
        let mut replay = Replay::new(false);
        replay.ingest(1, &ServerEvent::Planets { planets: vec![] });
        let row = replay.ingest(2, &begin(0.0)).unwrap();
        assert_eq!(row["truth_status"], "unavailable");
        assert!(row["physical"].is_null());
    }
}
