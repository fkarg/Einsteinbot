//! Prefix-only replay of real captured paths; held-out shots are selected at launch,
//! not completion, so their learning snapshot cannot include evidence from their flight.
use einsteinbot::estimator::Estimator;
use einsteinbot::estimator::shot_position_rmse;
use einsteinbot::map::{PlanetMap, Vec2};
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters, simulate_targeting_shot};
use einsteinbot::strategy::{BotState, MapBelief, TargetingPolicy};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

#[derive(Debug)]
struct Evaluation {
    epoch: usize,
    prefix: usize,
    shot_index: usize,
    flat_rmse: f64,
    learned_rmse: f64,
    oracle_rmse: Option<f64>,
}
struct Checkpoint {
    prefix: usize,
    belief: MapBelief,
    selected: usize,
}
struct Flight {
    player_id: u32,
    angle: f64,
    velocity: f64,
    checkpoints: Vec<usize>,
}

fn replay(
    events: &[ServerEvent],
    checkpoints: &[usize],
    holdout: usize,
    benchmark: bool,
) -> Vec<Evaluation> {
    let mut estimator = Estimator::default();
    let mut state = BotState::default();
    let cancelled = AtomicBool::new(false);
    let flat = PlanetMap { planets: vec![] };
    let mut oracle: Option<PlanetMap> = None;
    let mut epoch = 0;
    let mut count = 0;
    let mut snapshots: Vec<Checkpoint> = Vec::new();
    if checkpoints.contains(&0) {
        snapshots.push(Checkpoint {
            prefix: 0,
            belief: MapBelief::new(flat.clone(), None),
            selected: 0,
        });
    }
    let mut flights: Vec<Flight> = Vec::new();
    let mut evaluations = Vec::new();
    let mut fit_ms = 0_u128;
    for event in events {
        state.observe(event);
        match event {
            ServerEvent::Planets { planets } => {
                // Planets is an epoch marker and scoring oracle, never fit input.
                if count > 0 {
                    println!("epoch={epoch} completed={count} cumulative_fit_ms={fit_ms}");
                    print_incomplete(epoch, &snapshots, &evaluations, holdout);
                }
                epoch += 1;
                estimator.reset();
                oracle = Some(PlanetMap {
                    planets: planets.clone(),
                });
                count = 0;
                fit_ms = 0;
                snapshots.clear();
                flights.clear();
                if checkpoints.contains(&0) {
                    snapshots.push(Checkpoint {
                        prefix: 0,
                        belief: MapBelief::new(flat.clone(), None),
                        selected: 0,
                    });
                }
            }
            ServerEvent::PlayerPosition { .. } | ServerEvent::PlayerLeave { .. } => {
                estimator.observe(event, &cancelled);
            }
            ServerEvent::ShotBegin {
                player_id,
                angle,
                velocity,
            } => {
                let mut selected = Vec::new();
                for (index, checkpoint) in snapshots.iter_mut().enumerate() {
                    if checkpoint.selected < holdout {
                        checkpoint.selected += 1;
                        selected.push(index);
                    }
                }
                flights.push(Flight {
                    player_id: *player_id,
                    angle: *angle,
                    velocity: *velocity,
                    checkpoints: selected,
                });
            }
            ServerEvent::ShotFin {
                player_id,
                angle,
                velocity,
                ..
            } => {
                count += 1;
                if let Some(index) = flights.iter().position(|flight| {
                    flight.player_id == *player_id
                        && flight.angle == *angle
                        && flight.velocity == *velocity
                }) {
                    let flight = flights.remove(index);
                    let shot = ShotObservation::try_from(event).expect("matched shot event");
                    for checkpoint in flight.checkpoints {
                        let snapshot = &snapshots[checkpoint];
                        let flat_rmse =
                            shot_position_rmse(&flat, &shot, WorldParameters::default());
                        let learned_rmse = shot_position_rmse(
                            &snapshot.belief.map,
                            &shot,
                            WorldParameters::default(),
                        );
                        if let (Ok(Some(flat_rmse)), Ok(Some(learned_rmse))) =
                            (flat_rmse, learned_rmse)
                        {
                            let oracle_rmse = oracle.as_ref().and_then(|map| {
                                shot_position_rmse(map, &shot, WorldParameters::default())
                                    .ok()
                                    .flatten()
                            });
                            evaluations.push(Evaluation {
                                epoch,
                                prefix: snapshot.prefix,
                                shot_index: count,
                                flat_rmse,
                                learned_rmse,
                                oracle_rmse,
                            });
                        }
                    }
                }
                // Once all requested snapshots exist, avoid additional expensive fits; shots
                // starting thereafter still use the frozen snapshots for scoring.
                if checkpoints.iter().any(|prefix| *prefix >= count) {
                    let started = Instant::now();
                    estimator.observe(event, &cancelled);
                    estimator.settle(&cancelled);
                    fit_ms += started.elapsed().as_millis();
                }
                if checkpoints.contains(&count) {
                    let belief = estimator
                        .belief()
                        .cloned()
                        .unwrap_or_else(|| MapBelief::new(flat.clone(), None));
                    println!(
                        "checkpoint epoch={epoch} prefix={count} inferred_planets={} cumulative_fit_ms={fit_ms}",
                        belief.map.planets.len()
                    );
                    if benchmark && count == 8 {
                        benchmark_targeting(&state, &belief, oracle.as_ref());
                    }
                    snapshots.push(Checkpoint {
                        prefix: count,
                        belief,
                        selected: 0,
                    });
                }
            }
            _ => {}
        }
    }
    println!("epoch={epoch} completed={count} cumulative_fit_ms={fit_ms}");
    print_incomplete(epoch, &snapshots, &evaluations, holdout);
    evaluations
}

fn benchmark_targeting(state: &BotState, belief: &MapBelief, oracle: Option<&PlanetMap>) {
    let snapshot = state.diagnostic_snapshot();
    let Some(id) = snapshot["own_id"].as_u64().map(|id| id as u32) else {
        println!("benchmark no own player identity");
        return;
    };
    let Some(start) = state.player_positions().get(&id) else {
        println!("benchmark no own position");
        return;
    };
    let players = state
        .player_positions()
        .iter()
        .map(|(&id, p)| (id, Vec2::new(f64::from(p.x), f64::from(p.y))))
        .collect::<Vec<_>>();
    let mut scenario = state.clone();
    scenario.set_map_belief(Some(belief.clone()));
    let started = Instant::now();
    let choice = TargetingPolicy::default().choose(&scenario, 20.0);
    let elapsed = started.elapsed().as_millis();
    if let Some(choice) = choice {
        let observed = oracle
            .and_then(|map| {
                simulate_targeting_shot(
                    map,
                    &ShotObservation {
                        player_id: id,
                        angle_degrees: f64::from(choice.angle_degrees),
                        velocity: f64::from(choice.velocity),
                        points: vec![start.clone()],
                    },
                    WorldParameters::default(),
                    2000,
                    &players,
                    4.0,
                )
                .ok()
            })
            .map(|path| path.termination);
        println!(
            "target_benchmark prefix=8 elapsed_ms={elapsed} choice={choice:?} actual_oracle_termination={observed:?}"
        );
    } else {
        println!("target_benchmark prefix=8 elapsed_ms={elapsed} no_choice");
    }
}

fn print_incomplete(
    epoch: usize,
    snapshots: &[Checkpoint],
    evaluations: &[Evaluation],
    holdout: usize,
) {
    for checkpoint in snapshots {
        let scored = evaluations
            .iter()
            .filter(|evaluation| {
                evaluation.epoch == epoch && evaluation.prefix == checkpoint.prefix
            })
            .count();
        if scored < holdout {
            println!(
                "checkpoint epoch={epoch} prefix={} selected={} scored={} requested={} (unfinished or unscorable paths are omitted)",
                checkpoint.prefix, checkpoint.selected, scored, holdout
            );
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut capture = 29_i64;
    let mut path = "einsteinbot.sqlite3".to_owned();
    let mut checkpoints = vec![0, 1, 4, 8, 16];
    let mut holdout = 3_usize;
    let mut benchmark = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capture-id" => capture = args.next().ok_or("missing capture id")?.parse()?,
            "--db-path" => path = args.next().ok_or("missing database path")?,
            "--checkpoints" => {
                checkpoints = args
                    .next()
                    .ok_or("missing comma-separated checkpoints")?
                    .split(',')
                    .map(str::parse)
                    .collect::<Result<Vec<usize>, _>>()?;
                checkpoints.sort_unstable();
                checkpoints.dedup();
            }
            "--holdout" => {
                holdout = args.next().ok_or("missing holdout count")?.parse()?;
                if holdout == 0 {
                    return Err("holdout must be positive".into());
                }
            }
            "--benchmark-targeting" => benchmark = true,
            "--help" | "-h" => {
                println!(
                    "Usage: capture_learning_scenarios [--capture-id ID] [--db-path PATH] [--checkpoints 0,1,4,8,16] [--holdout N] [--benchmark-targeting]\nSequentially fit only completed prefix trajectories. Each checkpoint scores the next N shots LAUNCHED after that snapshot when their ShotFin arrives; earlier in-flight shots cannot leak into the snapshot. Disclosed planets score predictions only and define epochs. Scores use the physical planet map only. Empty maps are evaluated as flat. Cumulative fit time excludes read and scoring time. Replays fit synchronously and do not reconstruct actual asynchronous live publications.\nScores exclude launch and terminal sample and are coordinate RMSE; full-path integrations do not stop at collisions. Unfinished shots and failed/inadequate comparisons are omitted. Multiple checkpoints can score the same held-out trajectory."
                );
                return Ok(());
            }
            _ => return Err(format!("unknown argument {arg}").into()),
        }
    }
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement =
        db.prepare("SELECT event_json FROM events WHERE capture_id=?1 ORDER BY sequence")?;
    let events = statement
        .query_map([capture], |row| row.get::<_, String>(0))?
        .map(|row| {
            let event: RecordedEvent = serde_json::from_str(&row?)?;
            Ok(match event {
                RecordedEvent::Inbound(event) => Some(event),
                _ => None,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if events.is_empty() {
        return Err("capture has no inbound events".into());
    }
    println!(
        "capture={capture} checkpoints={checkpoints:?} holdout={holdout}; prefix replay uses only completed observations, oracle only scores"
    );
    for result in replay(&events, &checkpoints, holdout, benchmark) {
        println!(
            "heldout epoch={} prefix={} completion_index={} flat_rmse={:.4} planet_map_only_rmse={:.4} oracle_rmse={:?}",
            result.epoch,
            result.prefix,
            result.shot_index,
            result.flat_rmse,
            result.learned_rmse,
            result.oracle_rmse
        );
    }
    Ok(())
}
fn main() {
    if let Err(error) = run() {
        eprintln!("capture_learning_scenarios: {error}");
        std::process::exit(2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use einsteinbot::protocol::Point;
    #[test]
    fn checkpoint_scores_a_future_path_without_fitting_it() {
        let begin = |velocity| ServerEvent::ShotBegin {
            player_id: 1,
            angle: 0.0,
            velocity,
        };
        let event = |velocity: f64| ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity,
            points: vec![
                Point { x: 0.0, y: 0.0 },
                Point {
                    x: velocity as f32,
                    y: 0.0,
                },
                Point {
                    x: 2.0 * velocity as f32,
                    y: 0.0,
                },
                Point {
                    x: 3.0 * velocity as f32,
                    y: 0.0,
                },
            ],
        };
        let reports = replay(
            &[begin(2.0), event(2.0), begin(3.0), event(3.0)],
            &[1],
            1,
            false,
        );
        assert_eq!(reports.len(), 1);
        assert_eq!((reports[0].prefix, reports[0].shot_index), (1, 2));
        assert!(reports[0].flat_rmse < 1e-12);
        assert!(reports[0].learned_rmse < 1e-12);
    }
    #[test]
    fn pre_checkpoint_inflight_shot_is_not_a_holdout() {
        let begin = |velocity| ServerEvent::ShotBegin {
            player_id: 1,
            angle: 0.0,
            velocity,
        };
        let finish = |velocity: f64| ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity,
            points: vec![
                Point { x: 0.0, y: 0.0 },
                Point {
                    x: velocity as f32,
                    y: 0.0,
                },
                Point {
                    x: 2.0 * velocity as f32,
                    y: 0.0,
                },
                Point {
                    x: 3.0 * velocity as f32,
                    y: 0.0,
                },
            ],
        };
        let reports = replay(
            &[
                begin(2.0),
                begin(3.0),
                finish(2.0),
                finish(3.0),
                begin(4.0),
                finish(4.0),
            ],
            &[1],
            1,
            false,
        );
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].shot_index, 3);
    }
}
