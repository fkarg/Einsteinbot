use einsteinbot::protocol::{Point, RecordedEvent, ServerEvent};
use std::collections::BTreeSet;

#[derive(Default)]
struct Coverage {
    cells: BTreeSet<(i32, i32)>,
}
impl Coverage {
    fn observe(&mut self, points: &[Point]) -> Option<f64> {
        let cells = einsteinbot::coverage::path_cells(&points[..points.len().saturating_sub(1)]);
        if cells.is_empty() {
            return None;
        }
        let overlap = cells.intersection(&self.cells).count() as f64 / cells.len() as f64;
        self.cells.extend(cells);
        Some(overlap)
    }
}
fn main() {
    if let Err(error) = run() {
        eprintln!("targeting_report failed: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut path = "einsteinbot.sqlite3".to_owned();
    let mut capture = None;
    let mut compare = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capture-id" => {
                capture = Some(args.next().ok_or("missing capture id")?.parse::<i64>()?)
            }
            "--compare-policy" => compare = true,
            "--db-path" => path = args.next().ok_or("missing database path")?,
            "--help" | "-h" => {
                println!(
                    "Usage: targeting_report --capture-id ID [--db-path PATH] [--compare-policy]"
                );
                return Ok(());
            }
            _ => return Err(format!("unknown option {arg}").into()),
        }
    }
    let capture = capture.ok_or("--capture-id is required")?;
    // Explicit read-only mode: diagnostics must never create or mutate a capture database.
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut query = db
        .prepare("SELECT bot_name, event_json FROM events WHERE capture_id=?1 ORDER BY sequence")?;
    let events = query
        .query_map([capture], |r| {
            Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|row| {
            let (name, json) = row?;
            Ok((name, serde_json::from_str::<RecordedEvent>(&json)?))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    if events.is_empty() {
        return Err("capture has no events".into());
    }
    if compare {
        compare_policy(&events);
    }
    let mut epoch = 0;
    let mut report = Report::default();
    for (_, event) in events {
        match event {
            RecordedEvent::Inbound(ServerEvent::Planets { planets }) => {
                if report.shots > 0 {
                    report.print(capture, epoch);
                }
                epoch += 1;
                report = Report {
                    oracle: Some(einsteinbot::map::PlanetMap { planets }),
                    ..Report::default()
                };
            }
            RecordedEvent::Inbound(event @ ServerEvent::ShotFin { .. }) => report.observe(&event),
            _ => {}
        }
    }
    report.print(capture, epoch);
    println!(
        "Cells are 20 world units; overlap uses distinct segment-rasterized interior cells against prior completed paths in the same epoch. All ShotFin paths include debris; these counts are not player commands or hit rates. Oracle RMSE excludes launch and untimed terminal samples; collisions can truncate comparisons."
    );
    Ok(())
}

#[derive(Default)]
struct Comparison {
    decisions: usize,
    changed: usize,
    hits: usize,
    changed_hits: usize,
    baseline_coverage: f64,
    coverage: f64,
    baseline_miss: f64,
    miss: f64,
}
fn compare_policy(events: &[(Option<String>, RecordedEvent)]) -> Comparison {
    use einsteinbot::strategy::{
        BotState, MapBelief, ShotReason, TargetingPolicy, TargetingSettings,
    };
    use std::collections::BTreeMap;
    let mut states: BTreeMap<String, (BotState, usize)> = BTreeMap::new();
    let mut world = Vec::new();
    let mut result = Comparison::default();
    for (name, recorded) in events {
        let RecordedEvent::Inbound(event) = recorded else {
            continue;
        };
        if matches!(
            event,
            ServerEvent::OwnId { .. } | ServerEvent::OwnEnergy { .. }
        ) {
            let key = name.clone().unwrap_or_else(|| "legacy".to_owned());
            if !states.contains_key(&key) {
                let mut state = BotState::default();
                for prior in &world {
                    state.observe(prior);
                    if let ServerEvent::Planets { planets } = prior {
                        state.set_map_belief(Some(MapBelief::new(
                            einsteinbot::map::PlanetMap {
                                planets: planets.clone(),
                            },
                            None,
                        )));
                    }
                }
                let index = states.len();
                states.insert(key.clone(), (state, index));
            }
            let (state, index) = states.get_mut(&key).unwrap();
            state.observe(event);
            if let ServerEvent::OwnEnergy { energy, .. } = event {
                // Oracle-only diagnostic: no future paths or fitted map enter this decision.
                if !world
                    .iter()
                    .any(|event| matches!(event, ServerEvent::Planets { .. }))
                {
                    continue;
                }
                let mut settings = TargetingSettings::default();
                settings.angle_step_degrees = [8, 10, 12][*index % 3];
                let proposal = TargetingPolicy::new(settings).choose(state, *energy);
                settings.coverage_penalty = 0.0;
                let baseline = TargetingPolicy::new(settings).choose(state, *energy);
                if let (Some(proposal), Some(baseline)) = (proposal, baseline) {
                    result.decisions += 1;
                    result.changed += usize::from(
                        (proposal.velocity, proposal.angle_degrees)
                            != (baseline.velocity, baseline.angle_degrees),
                    );
                    result.hits += usize::from(baseline.reason == ShotReason::Hit);
                    result.changed_hits += usize::from(
                        baseline.reason == ShotReason::Hit
                            && (proposal.velocity, proposal.angle_degrees)
                                != (baseline.velocity, baseline.angle_degrees),
                    );
                    result.baseline_coverage += baseline.covered_fraction;
                    result.coverage += proposal.covered_fraction;
                    result.baseline_miss += baseline.miss_distance;
                    result.miss += proposal.miss_distance;
                }
            }
        } else {
            world.push(event.clone());
            for (state, _) in states.values_mut() {
                state.observe(event);
                if let ServerEvent::Planets { planets } = event {
                    state.set_map_belief(Some(MapBelief::new(
                        einsteinbot::map::PlanetMap {
                            planets: planets.clone(),
                        },
                        None,
                    )));
                }
            }
        }
    }
    let n = result.decisions.max(1) as f64;
    println!(
        "oracle_policy_comparison decisions={} changed={} baseline_hits={} changed_hits={} mean_overlap_baseline={:.6} mean_overlap_proposal={:.6} mean_miss_baseline={:.6} mean_miss_proposal={:.6}",
        result.decisions,
        result.changed,
        result.hits,
        result.changed_hits,
        result.baseline_coverage / n,
        result.coverage / n,
        result.baseline_miss / n,
        result.miss / n
    );
    println!(
        "Counterfactual choices use recorded histories and oracle maps, with 8/10/12 degree profiles assigned in first-identity order. They do not generate new observations, model live estimator errors, or establish win rates."
    );
    result
}

#[derive(Default)]
struct Report {
    coverage: Coverage,
    signatures: BTreeSet<(u32, u32, u64, u64)>,
    oracle: Option<einsteinbot::map::PlanetMap>,
    shots: usize,
    exact_repeats: usize,
    short_paths: usize,
    overlaps: Vec<f64>,
    oracle_errors: Vec<f64>,
    oracle_failures: usize,
}
impl Report {
    fn observe(&mut self, event: &ServerEvent) {
        let Ok(shot) = einsteinbot::simulation::ShotObservation::try_from(event) else {
            return;
        };
        self.shots += 1;
        if shot.points.len() <= 3 {
            self.short_paths += 1;
        }
        if let Some(start) = shot.points.first() {
            if !self.signatures.insert((
                start.x.to_bits(),
                start.y.to_bits(),
                shot.velocity.to_bits(),
                shot.angle_degrees.to_bits(),
            )) {
                self.exact_repeats += 1;
            }
        }
        if let Some(overlap) = self.coverage.observe(&shot.points) {
            self.overlaps.push(overlap);
        }
        if let Some(map) = &self.oracle {
            match einsteinbot::estimator::shot_position_rmse(
                map,
                &shot,
                einsteinbot::simulation::WorldParameters::default(),
            ) {
                Ok(Some(error)) if error.is_finite() => self.oracle_errors.push(error),
                _ => self.oracle_failures += 1,
            }
        }
    }
    fn print(&self, capture: i64, epoch: usize) {
        println!(
            "capture={capture} epoch={epoch} shots={} exact_repeats={} short_paths_le3={} coverage_cells={} scored_overlap={} median_overlap={} overlap_ge80pct={} oracle_scored={} oracle_median_rmse={} oracle_unscored={}",
            self.shots,
            self.exact_repeats,
            self.short_paths,
            self.coverage.cells.len(),
            self.overlaps.len(),
            median(&self.overlaps),
            self.overlaps.iter().filter(|x| **x >= 0.8).count(),
            self.oracle_errors.len(),
            median(&self.oracle_errors),
            self.oracle_failures
        );
    }
}
fn median(values: &[f64]) -> String {
    if values.is_empty() {
        return "unavailable".to_owned();
    }
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    let middle = values.len() / 2;
    let result = if values.len() % 2 == 0 {
        (values[middle - 1] + values[middle]) / 2.0
    } else {
        values[middle]
    };
    format!("{result:.6}")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn points(xs: &[f32]) -> Vec<Point> {
        xs.iter().map(|x| Point { x: *x, y: 1.0 }).collect()
    }
    #[test]
    fn oracle_replay_compares_policies_on_the_same_observed_history() {
        let events = vec![
            ServerEvent::Planets { planets: vec![] },
            ServerEvent::OwnId { player_id: 1 },
            ServerEvent::PlayerPosition {
                player_id: 1,
                x: 100.0,
                y: 100.0,
            },
            ServerEvent::PlayerPosition {
                player_id: 2,
                x: 1100.0,
                y: 100.0,
            },
            ServerEvent::ShotFin {
                player_id: 1,
                velocity: 3.0,
                angle: 0.0,
                points: vec![
                    Point { x: 100.0, y: 100.0 },
                    Point { x: 460.0, y: 100.0 },
                    Point { x: 461.0, y: 100.0 },
                ],
            },
            ServerEvent::OwnEnergy {
                player_id: 1,
                energy: 2.0,
            },
        ]
        .into_iter()
        .map(|event| (None, RecordedEvent::Inbound(event)))
        .collect::<Vec<_>>();
        let result = compare_policy(&events);
        assert_eq!(result.decisions, 1);
        assert_eq!(result.changed, 1);
        assert_eq!(result.hits, 0);
        assert!(result.coverage < result.baseline_coverage);
    }

    #[test]
    fn exact_repeats_are_separate_from_spatial_overlap() {
        let event = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.0,
            velocity: 2.0,
            points: points(&[0., 21., 41., 100.]),
        };
        let mut report = Report::default();
        report.observe(&event);
        report.observe(&event);
        assert_eq!(report.exact_repeats, 1);
        assert_eq!(report.overlaps, vec![0.0, 1.0]);
        report = Report::default();
        report.observe(&event);
        assert_eq!(report.exact_repeats, 0);
        assert_eq!(report.overlaps, vec![0.0]);
    }

    #[test]
    fn overlap_uses_distinct_interior_cells_and_epoch_reset_forgets_them() {
        // Shared launch and terminal points must not fabricate observed-path coverage.
        let mut coverage = Coverage::default();
        assert_eq!(
            coverage.observe(&points(&[0., 21., 22., 41., 100.])),
            Some(0.)
        );
        assert_eq!(
            coverage.observe(&points(&[0., 21., 61., 100.])),
            Some(2.0 / 3.0)
        );
        coverage = Coverage::default();
        assert_eq!(coverage.observe(&points(&[0., 21., 61., 100.])), Some(0.));
        assert_eq!(coverage.observe(&points(&[0., 100.])), None);
    }
}
