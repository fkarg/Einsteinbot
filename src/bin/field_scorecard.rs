//! Paired field evaluation from an ordered capture; truth is used only for scoring.
use einsteinbot::estimator::SeedStrategy;
use einsteinbot::event_store::EventLog;
use einsteinbot::field_score::{FieldModel, score_field};
use einsteinbot::map::PlanetMap;
use einsteinbot::online_map::OnlineEstimator;
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::MapBelief;
use serde_json::{Value, json};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

struct Branch {
    name: &'static str,
    estimator: OnlineEstimator,
    fit_micros: u128,
}

struct Replay {
    branches: Vec<Branch>,
    observations: Vec<ShotObservation>,
    truth: Option<PlanetMap>,
    epoch: usize,
    completed_total: usize,
    empty_is_flat: bool,
}

impl Replay {
    fn new(grid: bool, bearing: bool, empty_is_flat: bool) -> Self {
        let branches = [
            ("grid", SeedStrategy::Grid, grid),
            ("bearing", SeedStrategy::Bearing, bearing),
        ]
        .into_iter()
        .filter(|(_, _, enabled)| *enabled)
        .map(|(name, strategy, _)| Branch {
            name,
            estimator: OnlineEstimator::with_seed_strategy(strategy).with_provisional_field(),
            fit_micros: 0,
        })
        .collect();
        Self {
            branches,
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
                for branch in &mut self.branches {
                    branch.estimator.reset();
                    branch.fit_micros = 0;
                }
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
                for branch in &mut self.branches {
                    let start = Instant::now();
                    branch.estimator.observe(event, &AtomicBool::new(false));
                    branch.fit_micros += start.elapsed().as_micros();
                }
                self.observations.push(shot);
                self.completed_total += 1;
                let mut row = self.snapshot(sequence, "after_completion");
                row["shot"] = json!({"player_id": player_id, "angle": angle, "velocity": velocity});
                Some(row)
            }
            ServerEvent::PlayerPosition { .. } | ServerEvent::PlayerLeave { .. } => {
                for branch in &mut self.branches {
                    branch.estimator.observe(event, &AtomicBool::new(false));
                }
                None
            }
            _ => None,
        }
    }

    fn snapshot(&self, sequence: i64, phase: &str) -> Value {
        let mut row = json!({ "sequence": sequence, "epoch": self.epoch, "phase": phase,
            "completed_paths": self.observations.len(),
            "completed_total": self.completed_total,
            "truth_status": if self.truth.is_some() { "disclosed" } else { "unavailable" },
            "grid": null, "bearing": null });
        for branch in &self.branches {
            let empty = MapBelief::new(PlanetMap { planets: vec![] }, None);
            let belief = branch.estimator.belief().unwrap_or(&empty);
            let world = WorldParameters::default();
            let validated_status = if belief.field_hypothesis.is_none() {
                "no_validated_source"
            } else if belief.hypothesis_local_field.is_none() {
                "matching_residual_unavailable"
            } else {
                "available"
            };
            let provisional_status = if belief.provisional_field_hypothesis.is_some() {
                "available"
            } else {
                "no_provisional_source"
            };
            let Some(truth) = &self.truth else {
                row[branch.name] = json!({ "fit_ms": branch.fit_micros as f64 / 1000.0,
                    "belief_published": branch.estimator.belief().is_some(), "physical_planets": belief.map.planets.len(), "physical": null, "no_source": null,
                    "validated": null, "validated_source_only": null, "provisional": null, "provisional_source_only": null, "flat": null, "oracle": null,
                    "validated_status": validated_status, "provisional_status": provisional_status });
                continue;
            };
            row[branch.name] = json!({ "fit_ms": branch.fit_micros as f64 / 1000.0, "belief_published": branch.estimator.belief().is_some(), "physical_planets": belief.map.planets.len(),
                "validated_status": validated_status, "provisional_status": provisional_status,
                "physical": score_field(truth, belief, &self.observations, world, FieldModel::Physical),
                "no_source": score_field(truth, belief, &self.observations, world, FieldModel::NoSource),
                "validated": score_field(truth, belief, &self.observations, world, FieldModel::Validated),
                "validated_source_only": score_field(truth, belief, &self.observations, world, FieldModel::ValidatedSourceOnly),
                "provisional": score_field(truth, belief, &self.observations, world, FieldModel::Provisional),
                "provisional_source_only": score_field(truth, belief, &self.observations, world, FieldModel::ProvisionalSourceOnly),
                "flat": score_field(truth, &empty, &self.observations, world, FieldModel::Physical),
                "oracle": score_field(truth, &MapBelief::new(truth.clone(), None), &self.observations, world, FieldModel::Physical),
            });
        }
        if self.branches.len() == 2 {
            for model in [
                "physical",
                "no_source",
                "validated",
                "validated_source_only",
                "provisional",
                "provisional_source_only",
            ] {
                for region in ["explored", "unexplored", "whole"] {
                    row["bearing_minus_grid"][model][region] = paired_region(
                        self.truth.is_some(),
                        &row["grid"][model],
                        &row["bearing"][model],
                        region,
                    );
                }
            }
        }
        row
    }
}

fn paired_region(
    truth_available: bool,
    grid_model: &Value,
    bearing_model: &Value,
    region: &str,
) -> Value {
    let grid = &grid_model[region];
    let bearing = &bearing_model[region];
    let n_grid = grid["samples"].as_u64();
    let n_bearing = bearing["samples"].as_u64();
    let a = grid["vector_rmse"].as_f64();
    let b = bearing["vector_rmse"].as_f64();
    let status = if !truth_available {
        "truth_unavailable"
    } else if grid_model.is_null() {
        "grid_model_unavailable"
    } else if bearing_model.is_null() {
        "bearing_model_unavailable"
    } else if grid.is_null() || bearing.is_null() {
        "empty_region"
    } else if n_grid != n_bearing {
        "mask_mismatch"
    } else if a.is_none() || b.is_none() {
        "invalid_prediction"
    } else {
        "available"
    };
    json!({"status": status, "paired_cells": if n_grid == n_bearing { n_grid } else { None },
        "vector_rmse": if status == "available" { a.zip(b).map(|(grid,bearing)| bearing-grid) } else { None }})
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let mut db = String::from("learning-scenarios.sqlite3");
    let mut capture: Option<i64> = None;
    let mut seed = String::from("both");
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
            "--seed" => seed = args.next().ok_or("--seed requires grid, bearing or both")?,
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
                    "Usage: field_scorecard --capture-id ID [--db-path PATH] [--seed grid|bearing|both] [--empty-is-flat] [--max-completed N]\nJSON lines report causal pre-launch and post-completion field scores. Empty planet messages are unknown unless --empty-is-flat is explicitly set. The live default is grid; bearing is offline experimental."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let capture = capture.ok_or("--capture-id is required")?;
    if !["grid", "bearing", "both"].contains(&seed.as_str()) {
        return Err("--seed requires grid, bearing or both".into());
    }
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
    let mut replay = Replay::new(seed != "bearing", seed != "grid", empty_is_flat);
    println!(
        "{}",
        json!({"type":"field_scorecard", "capture_id":capture, "db_path":db, "seed":seed,
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

    #[test]
    fn paired_difference_reports_invalid_and_unmatched_samples_explicitly() {
        let good = json!({"whole": {"samples": 3, "vector_rmse": 2.0}});
        let better = json!({"whole": {"samples": 3, "vector_rmse": 1.0}});
        assert_eq!(
            paired_region(true, &good, &better, "whole"),
            json!({"status":"available", "paired_cells":3, "vector_rmse":-1.0})
        );
        let invalid = json!({"whole": {"samples": 3, "invalid": 1, "vector_rmse": null}});
        assert_eq!(
            paired_region(true, &good, &invalid, "whole")["status"],
            "invalid_prediction"
        );
        let different = json!({"whole": {"samples": 2, "vector_rmse": 1.0}});
        assert_eq!(
            paired_region(true, &good, &different, "whole")["status"],
            "mask_mismatch"
        );
        assert_eq!(
            paired_region(true, &good, &Value::Null, "whole")["status"],
            "bearing_model_unavailable"
        );
    }

    #[test]
    fn launch_snapshot_excludes_shots_that_finish_after_launch_and_resets_per_epoch() {
        let mut replay = Replay::new(true, true, true);
        replay.ingest(1, &ServerEvent::Planets { planets: vec![] });
        let initial = replay
            .ingest(
                2,
                &ServerEvent::ShotBegin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                },
            )
            .unwrap();
        assert_eq!(initial["completed_paths"], 0);
        assert!(initial["grid"]["no_source"]["explored"].is_null());
        assert_eq!(
            initial["bearing_minus_grid"]["physical"]["explored"]["status"],
            "empty_region"
        );
        assert_eq!(
            initial["bearing_minus_grid"]["provisional"]["explored"]["status"],
            "grid_model_unavailable"
        );
        replay.ingest(
            3,
            &ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.0,
                velocity: 8.0,
                points: vec![
                    Point { x: 1.0, y: 1.0 },
                    Point { x: 41.0, y: 1.0 },
                    Point { x: 41.0, y: 1.0 },
                ],
            },
        );
        let later = replay
            .ingest(
                4,
                &ServerEvent::ShotBegin {
                    player_id: 1,
                    angle: 45.0,
                    velocity: 8.0,
                },
            )
            .unwrap();
        assert_eq!(later["completed_paths"], 1);
        assert_eq!(
            later["bearing_minus_grid"]["no_source"]["explored"]["status"],
            "available"
        );
        assert_eq!(
            later["bearing_minus_grid"]["no_source"]["explored"]["vector_rmse"],
            0.0
        );
        assert!(
            later["grid"]["no_source"]["explored"]["samples"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(
            later["grid"]["no_source"]["explored"]["samples"],
            later["bearing"]["no_source"]["explored"]["samples"]
        );
        replay.ingest(5, &ServerEvent::Planets { planets: vec![] });
        let reset = replay
            .ingest(
                6,
                &ServerEvent::ShotBegin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                },
            )
            .unwrap();
        assert_eq!(reset["completed_paths"], 0);
        assert_eq!(reset["completed_total"], 1);
        assert_eq!(reset["epoch"], 2);
        assert!(reset["grid"]["physical"]["explored"].is_null());
        let after_reset = replay
            .ingest(
                7,
                &ServerEvent::ShotFin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                    points: vec![
                        Point { x: 1.0, y: 1.0 },
                        Point { x: 41.0, y: 1.0 },
                        Point { x: 41.0, y: 1.0 },
                    ],
                },
            )
            .unwrap();
        assert_eq!(after_reset["completed_paths"], 1);
        assert_eq!(after_reset["completed_total"], 2);
    }

    #[test]
    fn empty_planets_are_not_implicitly_a_disclosed_flat_world() {
        let mut replay = Replay::new(true, false, false);
        replay.ingest(1, &ServerEvent::Planets { planets: vec![] });
        let row = replay
            .ingest(
                2,
                &ServerEvent::ShotBegin {
                    player_id: 1,
                    angle: 0.0,
                    velocity: 8.0,
                },
            )
            .unwrap();
        assert_eq!(row["truth_status"], "unavailable");
        assert!(row["grid"]["physical"].is_null());
        assert!(
            row.get("bearing_minus_grid").is_none(),
            "a single-variant replay has no paired delta"
        );
    }

    #[test]
    fn changing_oracle_planets_does_not_change_flyby_estimation() {
        use einsteinbot::protocol::Planet;
        let source = Planet {
            x: 600.0,
            y: 500.0,
            radius: 30.0,
            mass: 2700.0,
        };
        let truth = PlanetMap {
            planets: vec![source.clone()],
        };
        let launch = ShotObservation {
            player_id: 1,
            angle_degrees: 0.0,
            velocity: 8.0,
            points: vec![Point { x: 200.0, y: 400.0 }],
        };
        let path = einsteinbot::simulation::simulate_shot(
            &truth,
            &launch,
            WorldParameters::default(),
            2000,
        )
        .unwrap();
        let mut a = Replay::new(true, false, false);
        let mut b = Replay::new(true, false, false);
        a.ingest(
            1,
            &ServerEvent::Planets {
                planets: vec![source],
            },
        );
        b.ingest(
            1,
            &ServerEvent::Planets {
                planets: vec![Planet {
                    x: 900.0,
                    y: 700.0,
                    radius: 25.0,
                    mass: 1562.5,
                }],
            },
        );
        let event = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 8.0,
            points: path.points,
        };
        a.ingest(2, &event);
        b.ingest(2, &event);
        let belief_a = a.branches[0].estimator.belief().unwrap();
        let belief_b = b.branches[0].estimator.belief().unwrap();
        assert_eq!(belief_a.map.planets, belief_b.map.planets);
        assert!(belief_a.provisional_field_hypothesis.is_some());
        assert_eq!(
            serde_json::to_value(&belief_a.provisional_field_hypothesis).unwrap(),
            serde_json::to_value(&belief_b.provisional_field_hypothesis).unwrap()
        );

        // The estimator itself uses Planets only as an epoch boundary, including
        // when its non-default initializer is active.
        let mut direct_a =
            OnlineEstimator::with_seed_strategy(SeedStrategy::Bearing).with_provisional_field();
        let mut direct_b =
            OnlineEstimator::with_seed_strategy(SeedStrategy::Bearing).with_provisional_field();
        let cancel = AtomicBool::new(false);
        direct_a.observe(
            &ServerEvent::Planets {
                planets: truth.planets,
            },
            &cancel,
        );
        direct_b.observe(&ServerEvent::Planets { planets: vec![] }, &cancel);
        direct_a.observe(&event, &cancel);
        direct_b.observe(&event, &cancel);
        assert_eq!(direct_a.seed_strategy(), SeedStrategy::Bearing);
        let left = &direct_a.belief().unwrap().provisional_field_hypothesis;
        let right = &direct_b.belief().unwrap().provisional_field_hypothesis;
        assert!(left.is_some());
        assert_eq!(
            serde_json::to_value(left).unwrap(),
            serde_json::to_value(right).unwrap()
        );
    }
}
