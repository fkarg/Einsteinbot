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
    let mut fit_planets = false;
    let mut batch_shots = 1_usize;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--capture-id" => {
                capture = Some(args.next().ok_or("missing capture id")?.parse::<i64>()?)
            }
            "--compare-policy" => compare = true,
            "--fit-planets" => fit_planets = true,
            "--batch-shots" => {
                batch_shots = args.next().ok_or("missing batch size")?.parse()?;
                if batch_shots == 0 {
                    return Err("--batch-shots must be positive".into());
                }
            }
            "--db-path" => path = args.next().ok_or("missing database path")?,
            "--help" | "-h" => {
                println!(
                    "Usage: targeting_report --capture-id ID [--db-path PATH] [--compare-policy] [--fit-planets] [--batch-shots N]"
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
    if fit_planets {
        let mut replay = PlanetReplay {
            batch_shots,
            ..PlanetReplay::default()
        };
        for (_, event) in &events {
            if let RecordedEvent::Inbound(event) = event {
                if let Some(belief) = replay.observe(event) {
                    replay.print_fit(&belief);
                }
            }
        }
        if let Some(belief) = replay.flush() {
            replay.print_fit(&belief);
        }
        replay.print_summary();
        println!(
            "Planet replay performs prefix-only fits from positions and completed trajectories, flushing every configured number of distinct paths and at epoch/end boundaries. All paths within a pending batch use the preceding published map for prediction scoring. Disclosed Planets only reset epochs and score outputs, never enter the estimator. Matched planets use deterministic maximum-cardinality one-to-one center matching within 50 units (neighbors ordered by distance, not minimum-total-distance matching); unmatched inferred planets count as unresolved/false hypotheses, unmatched truth as undiscovered. Reported fit RMSE is training error. Future RMSE scores each newly completed trajectory against the map published before learning that trajectory; early empty maps and invalid comparisons are unscored. Flat-field RMSE uses exactly the same scored trajectories. Epoch resets clear all prediction and fit metrics; contact refinement that changes the fitted map clears its covariance/convergence/model-error metadata rather than attaching old-fit metrics to new geometry."
        );
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
        "Counterfactual choices use recorded histories and oracle maps, with 8/10/12 degree profiles assigned in first-identity order. Both policies retain the same contact handling; only coverage scoring differs. They do not generate new observations, model live estimator errors, or establish win rates."
    );
    result
}

#[derive(Default)]
struct PlanetReplay {
    estimator: einsteinbot::online_map::OnlineEstimator,
    epoch: usize,
    oracle: Option<einsteinbot::map::PlanetMap>,
    fits: usize,
    total_fits: usize,
    shots: usize,
    errors: Vec<f64>,
    last_map: Option<einsteinbot::map::PlanetMap>,
    future_errors: Vec<f64>,
    future_flat_errors: Vec<f64>,
    future_unscored: usize,
    batch_shots: usize,
    pending: Vec<ServerEvent>,
    pending_distinct: usize,
    seen: Vec<einsteinbot::simulation::ShotObservation>,
}
impl PlanetReplay {
    fn flush(&mut self) -> Option<einsteinbot::strategy::MapBelief> {
        let events = std::mem::take(&mut self.pending);
        self.pending_distinct = 0;
        let belief = self
            .estimator
            .observe_batch(&events, &std::sync::atomic::AtomicBool::new(false))?;
        self.fits += 1;
        self.total_fits += 1;
        if let Some(error) = belief.model_position_rmse {
            self.errors.push(error);
        }
        self.last_map = Some(belief.map.clone());
        Some(belief)
    }
    fn observe(&mut self, event: &ServerEvent) -> Option<einsteinbot::strategy::MapBelief> {
        if let ServerEvent::Planets { planets } = event {
            if let Some(belief) = self.flush() {
                self.print_fit(&belief);
            }
            if self.epoch > 0 || self.fits > 0 {
                self.print_summary();
            }
            self.estimator.reset();
            self.epoch += 1;
            self.oracle = Some(einsteinbot::map::PlanetMap {
                planets: planets.clone(),
            });
            self.fits = 0;
            self.shots = 0;
            self.errors.clear();
            self.last_map = None;
            self.future_errors.clear();
            self.future_flat_errors.clear();
            self.future_unscored = 0;
            self.seen.clear();
            return None;
        }
        if matches!(event, ServerEvent::ShotFin { .. }) {
            self.shots += 1;
            // Evaluate this newly completed path before supplying it to the estimator.
            // Empty/no map is not counted as a learned prediction.
            let prediction = self
                .estimator
                .belief()
                .filter(|belief| !belief.map.planets.is_empty())
                .and_then(|belief| {
                    let shot = einsteinbot::simulation::ShotObservation::try_from(event).ok()?;
                    let world = einsteinbot::simulation::WorldParameters::default();
                    let error =
                        einsteinbot::estimator::shot_position_rmse(&belief.map, &shot, world)
                            .ok()??;
                    let flat = einsteinbot::estimator::shot_position_rmse(
                        &einsteinbot::map::PlanetMap { planets: vec![] },
                        &shot,
                        world,
                    )
                    .ok()??;
                    Some((error, flat))
                })
                .filter(|(error, flat)| error.is_finite() && flat.is_finite());
            if let Some((error, flat)) = prediction {
                self.future_flat_errors.push(flat);
                self.future_errors.push(error);
                println!(
                    "planet_prediction epoch={} next_shot={} preupdate_rmse={error:.6} flat_rmse={flat:.6}",
                    self.epoch, self.shots
                );
            } else {
                self.future_unscored += 1;
            }
        }
        if matches!(
            event,
            ServerEvent::PlayerPosition { .. }
                | ServerEvent::PlayerLeave { .. }
                | ServerEvent::ShotFin { .. }
        ) {
            self.pending.push(event.clone());
            if let Ok(shot) = einsteinbot::simulation::ShotObservation::try_from(event) {
                if !self.seen.contains(&shot) {
                    self.seen.push(shot);
                    self.pending_distinct += 1;
                }
            }
        }
        if self.pending_distinct >= self.batch_shots.max(1) {
            self.flush()
        } else {
            None
        }
    }
    fn print_fit(&self, belief: &einsteinbot::strategy::MapBelief) {
        println!(
            "planet_fit epoch={} fit={} prefix_shots={} inferred={} ambiguous={} position_rmse={} covariance={} batch_shots={}",
            self.epoch,
            self.fits,
            self.shots,
            belief.map.planets.len(),
            belief.uncertain_associations,
            belief
                .model_position_rmse
                .map(|x| format!("{x:.6}"))
                .unwrap_or_else(|| "unavailable".to_owned()),
            belief.parameter_covariance.is_some(),
            self.batch_shots.max(1)
        );
        self.print_matching(&belief.map);
    }
    fn print_matching(&self, map: &einsteinbot::map::PlanetMap) {
        if let Some(oracle) = &self.oracle {
            let (center, radius, unmatched) = match_planets(map, oracle);
            println!(
                "planet_score epoch={} inferred={} truth={} matched={} unmatched_inferred={} undiscovered_truth={} median_center_error={} median_radius_error={}",
                self.epoch,
                map.planets.len(),
                oracle.planets.len(),
                center.len(),
                unmatched,
                oracle.planets.len() - center.len(),
                median(&center),
                median(&radius)
            );
        }
    }
    fn print_summary(&self) {
        println!(
            "planet_epoch epoch={} fits={} total_fits={} shots={} median_fit_position_rmse={} future_scored={} future_unscored={} median_future_position_rmse={} median_future_flat_rmse={}",
            self.epoch,
            self.fits,
            self.total_fits,
            self.shots,
            median(&self.errors),
            self.future_errors.len(),
            self.future_unscored,
            median(&self.future_errors),
            median(&self.future_flat_errors)
        );
        if let Some(map) = &self.last_map {
            self.print_matching(map);
        }
    }
}
fn match_planets(
    inferred: &einsteinbot::map::PlanetMap,
    truth: &einsteinbot::map::PlanetMap,
) -> (Vec<f64>, Vec<f64>, usize) {
    let neighbors = inferred
        .planets
        .iter()
        .map(|predicted| {
            let mut candidates = truth
                .planets
                .iter()
                .enumerate()
                .filter_map(|(j, actual)| {
                    let distance = ((predicted.x - actual.x).powi(2)
                        + (predicted.y - actual.y).powi(2))
                    .sqrt();
                    (distance <= 50.0).then_some((distance, j))
                })
                .collect::<Vec<_>>();
            candidates.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            candidates
        })
        .collect::<Vec<_>>();
    // Augmenting paths maximize match count. Distance only orders neighbors;
    // this deliberately does not minimize total assignment distance.
    fn augment(
        i: usize,
        neighbors: &[Vec<(f64, usize)>],
        seen: &mut [bool],
        assigned: &mut [Option<usize>],
    ) -> bool {
        for &(_, j) in &neighbors[i] {
            if seen[j] {
                continue;
            }
            seen[j] = true;
            if assigned[j].is_none() || augment(assigned[j].unwrap(), neighbors, seen, assigned) {
                assigned[j] = Some(i);
                return true;
            }
        }
        false
    }
    let mut assigned = vec![None; truth.planets.len()];
    for i in 0..inferred.planets.len() {
        augment(
            i,
            &neighbors,
            &mut vec![false; truth.planets.len()],
            &mut assigned,
        );
    }
    let mut centers = Vec::new();
    let mut radii = Vec::new();
    for (j, i) in assigned.into_iter().enumerate() {
        if let Some(i) = i {
            let predicted = &inferred.planets[i];
            let actual = &truth.planets[j];
            centers
                .push(((predicted.x - actual.x).powi(2) + (predicted.y - actual.y).powi(2)).sqrt());
            radii.push((predicted.radius - actual.radius).abs());
        }
    }
    let unmatched = inferred.planets.len() - centers.len();
    (centers, radii, unmatched)
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
    fn oracle_matching_reassigns_nearest_pair_to_preserve_two_valid_matches() {
        // Nearest-first consumes truth x=0 for x=24 and strands x=-26;
        // the valid assignment is -26→0 and 24→50, both within the 50-unit cutoff.
        use einsteinbot::map::PlanetMap;
        use einsteinbot::protocol::Planet;
        let planet = |x| Planet {
            x,
            y: 0.,
            radius: 20.,
            mass: 800.,
        };
        let truth = PlanetMap {
            planets: vec![planet(0.), planet(50.)],
        };
        let inferred = PlanetMap {
            planets: vec![planet(24.), planet(-26.)],
        };
        let (center, radius, unmatched) = match_planets(&inferred, &truth);
        assert_eq!(center, vec![26., 26.]);
        assert_eq!(radius, vec![0., 0.]);
        assert_eq!(unmatched, 0);
    }

    #[test]
    fn oracle_matching_counts_duplicate_and_distant_hypotheses_as_unmatched() {
        use einsteinbot::map::PlanetMap;
        use einsteinbot::protocol::Planet;
        let planet = |x| Planet {
            x,
            y: 0.,
            radius: 20.,
            mass: 800.,
        };
        let truth = PlanetMap {
            planets: vec![planet(100.)],
        };
        let inferred = PlanetMap {
            planets: vec![planet(101.), planet(102.), planet(300.)],
        };
        let (center, radius, unmatched) = match_planets(&inferred, &truth);
        assert_eq!(center, vec![1.]);
        assert_eq!(radius, vec![0.]);
        assert_eq!(unmatched, 2);
    }

    #[test]
    fn planet_replay_uses_paths_not_oracle_and_resets_physical_map() {
        use einsteinbot::map::{PlanetMap, Vec2};
        use einsteinbot::protocol::Planet;
        use einsteinbot::simulation::{ShotObservation, WorldParameters, simulate_shot};
        let truth = PlanetMap {
            planets: vec![Planet {
                x: 500.,
                y: 500.,
                radius: 30.,
                mass: 2700.,
            }],
        };
        let mut replay = PlanetReplay::default();
        replay.observe(&ServerEvent::Planets {
            planets: vec![Planet {
                x: 1500.,
                y: 100.,
                radius: 20.,
                mass: 800.,
            }],
        });
        assert!(
            replay.estimator.belief().is_none(),
            "oracle must never seed fit"
        );
        let shot = ShotObservation {
            player_id: 1,
            angle_degrees: 0.,
            velocity: 3.,
            points: vec![Point { x: 410., y: 500. }],
        };
        let path = simulate_shot(&truth, &shot, WorldParameters::default(), 180).unwrap();
        replay.observe(&ServerEvent::PlayerPosition {
            player_id: 1,
            x: 410.,
            y: 500.,
        });
        let belief = replay
            .observe(&ServerEvent::ShotFin {
                player_id: 1,
                angle: 0.,
                velocity: 3.,
                points: path.points.clone(),
            })
            .expect("trajectory seeds physical map");
        assert_eq!(belief.map.planets.len(), 1);
        assert!(
            replay.future_errors.is_empty(),
            "first trajectory cannot score against its own fitted map"
        );
        let event = ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.,
            velocity: 3.,
            points: path.points,
        };
        let expected = einsteinbot::estimator::shot_position_rmse(
            &belief.map,
            &ShotObservation::try_from(&event).unwrap(),
            WorldParameters::default(),
        )
        .unwrap()
        .unwrap();
        replay.observe(&event);
        assert_eq!(
            replay.future_errors,
            vec![expected],
            "evaluate only the pre-update map"
        );
        let expected_flat = einsteinbot::estimator::shot_position_rmse(
            &PlanetMap { planets: vec![] },
            &ShotObservation::try_from(&event).unwrap(),
            WorldParameters::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(replay.future_flat_errors, vec![expected_flat]);
        assert!(replay.future_errors[0] < replay.future_flat_errors[0]);
        let point = Vec2::new(410., 500.);
        let actual = truth.acceleration(point).unwrap();
        let inferred = belief.map.acceleration(point).unwrap();
        let error = (actual.x - inferred.x).powi(2) + (actual.y - inferred.y).powi(2);
        assert!(
            error < actual.norm_squared() / 10.,
            "inferred field improves on flat baseline"
        );
        replay.observe(&ServerEvent::Planets { planets: vec![] });
        assert!(replay.estimator.belief().is_none());
        assert!(replay.future_errors.is_empty());
        assert!(replay.future_flat_errors.is_empty());
    }

    #[test]
    fn batch_replay_waits_for_distinct_paths_and_flushes_a_partial_epoch() {
        let shot = |velocity| ServerEvent::ShotFin {
            player_id: 1,
            angle: 0.,
            velocity,
            points: vec![
                Point { x: 10., y: 20. },
                Point { x: 12., y: 20. },
                Point { x: 14., y: 20. },
            ],
        };
        let mut replay = PlanetReplay {
            batch_shots: 2,
            ..PlanetReplay::default()
        };
        assert!(replay.observe(&shot(2.)).is_none());
        assert!(
            replay.observe(&shot(2.)).is_none(),
            "duplicates cannot fill a batch"
        );
        assert!(
            replay.estimator.belief().is_none(),
            "no early or future fit"
        );
        assert!(replay.observe(&shot(3.)).is_some());
        assert_eq!(replay.fits, 1);
        assert!(replay.pending.is_empty());
        assert!(replay.observe(&shot(4.)).is_none());
        replay.observe(&ServerEvent::Planets { planets: vec![] });
        assert_eq!(
            replay.total_fits, 2,
            "epoch boundary must publish the partial batch before resetting"
        );
        assert!(replay.pending.is_empty());
        assert!(replay.seen.is_empty());
        assert!(replay.estimator.belief().is_none());
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
