use einsteinbot::estimator::{
    fit_greedy_source_counts, position_rmse, shot_observations, shot_position_rmse,
};
use einsteinbot::event_store::EventLog;
use einsteinbot::map::PlanetMap;
use einsteinbot::online_map::OnlineStageProfile;
use einsteinbot::online_profile::{ProfileSample, replay_capture};
use einsteinbot::protocol::{RecordedEvent, ServerEvent};
use einsteinbot::simulation::WorldParameters;
use indicatif::ProgressBar;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug)]
struct Options {
    db_path: PathBuf,
    capture_id: i64,
    max_sources: usize,
    radius_prior: f64,
    grid_spacing: f64,
    validation_every: usize,
    use_oracle: bool,
    online: bool,
    profile_events: bool,
    replay_speed: f64,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("estimate failed: {error}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<(), String> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        return Ok(());
    };
    let events = EventLog::read_capture(&options.db_path, options.capture_id)
        .map_err(|error| format!("cannot read capture {}: {error}", options.capture_id))?;
    if options.online {
        return run_online_profile(&events, &options);
    }
    if options.use_oracle
        && events.iter().any(|event| {
            matches!(
                &event.event,
                RecordedEvent::Inbound(ServerEvent::Planets { .. })
            )
        })
    {
        return run_oracle_diagnostic(&events, &options);
    }
    let shots = shot_observations(&events);
    if shots.is_empty() {
        return Err(format!(
            "capture {} has no completed protocol-version-8 shot paths",
            options.capture_id
        ));
    }

    let (training, validation) = split_shots(&shots, options.validation_every);
    let progress = ProgressBar::new_spinner();
    progress.enable_steady_tick(std::time::Duration::from_millis(100));
    progress.set_message(format!(
        "fitting up to {} source counts from {} training shots (finite-difference optimizer)",
        options.max_sources,
        training.len()
    ));
    let candidates = fit_greedy_source_counts(
        &training,
        &validation,
        WorldParameters::default(),
        options.max_sources,
        options.radius_prior,
        options.grid_spacing,
    )
    .map_err(|error| error.to_string())?;
    progress.finish_and_clear();

    println!(
        "capture={} events={} training_shots={} validation_shots={}",
        options.capture_id,
        events.len(),
        training.len(),
        validation.len()
    );
    for candidate in candidates.candidates {
        let result = candidate.result;
        let param_count = result.map.planets.len() * 3;
        println!(
            "sources={} train_position_rmse={:.6} validation_position_rmse={} jacobian_rank={}/{} covariance={} converged={}",
            result.map.planets.len(),
            result.residual_rmse,
            candidate
                .validation_rmse
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "unavailable".to_owned()),
            result.rank,
            param_count,
            if result.covariance.is_some() {
                "available"
            } else {
                "rank-deficient"
            },
            result.converged,
        );
        for (index, planet) in result.map.planets.iter().enumerate() {
            println!(
                "  planet[{index}] center=({:.3},{:.3}) radius_prior={:.3} mass={:.3}",
                planet.x, planet.y, planet.radius, planet.mass
            );
        }
    }
    println!("search_stopped={:?}", candidates.stop_reason);
    println!("World settings use NewtonWars source defaults; no oracle map is used for fitting.");
    println!(
        "RMSE is per-coordinate position error in world units; compare held-out scores first."
    );
    println!(
        "converged means the numerical optimizer stopped successfully, not that the map is correct."
    );
    Ok(())
}

fn run_online_profile(
    events: &[einsteinbot::event_store::StoredEvent],
    options: &Options,
) -> Result<(), String> {
    let started = Instant::now();
    let samples = replay_capture(events, options.replay_speed)?;
    let elapsed_ms = started.elapsed().as_millis();
    let active_count = samples.iter().filter(|sample| !sample.stale).count();
    let shot_fin_count = samples
        .iter()
        .filter(|sample| sample.event == "shot_fin")
        .count();
    let speed_label = if options.replay_speed == 0.0 {
        "unpaced".to_owned()
    } else {
        format!("{}x", options.replay_speed)
    };
    println!(
        "online_profile capture={} speed={} rows={} active={} shot_fin_rows={} elapsed_ms={elapsed_ms}",
        options.capture_id,
        speed_label,
        samples.len(),
        active_count,
        shot_fin_count
    );
    print_profile_summary(&samples);
    for sample in samples
        .iter()
        .filter(|sample| options.profile_events || sample.event == "shot_fin")
    {
        let stages = sample.stages;
        let pending = if sample.stale {
            "n/a".to_owned()
        } else {
            sample
                .submitted
                .saturating_sub(sample.processed)
                .to_string()
        };
        let stage_sum = stages.prepare
            + stages.planet_fit
            + stages.local_residual
            + stages.field_hypothesis
            + stages.hypothesis_residual;
        let other = sample.service_time.saturating_sub(stage_sum);
        println!(
            "  event seq={} recorded_unix_ms={} epoch={} watermark={} kind={} queue_ms={:.3} service_ms={:.3} prepare_ms={:.3} planet_fit_ms={:.3} local_residual_ms={:.3} field_hypothesis_ms={:.3} hypothesis_residual_ms={:.3} other_ms={:.3} backlog={} stale={}",
            sample.sequence,
            sample.recorded_at_unix_ms,
            sample.epoch,
            sample.watermark,
            sample.event,
            sample.queue_wait.as_secs_f64() * 1000.0,
            sample.service_time.as_secs_f64() * 1000.0,
            stages.prepare.as_secs_f64() * 1000.0,
            stages.planet_fit.as_secs_f64() * 1000.0,
            stages.local_residual.as_secs_f64() * 1000.0,
            stages.field_hypothesis.as_secs_f64() * 1000.0,
            stages.hypothesis_residual.as_secs_f64() * 1000.0,
            other.as_secs_f64() * 1000.0,
            pending,
            sample.stale,
        );
    }
    Ok(())
}

fn print_profile_summary(samples: &[ProfileSample]) {
    let mut groups = BTreeMap::<&str, Vec<&ProfileSample>>::new();
    for sample in samples {
        groups.entry(sample.event).or_default().push(sample);
    }
    for (event, samples) in groups {
        let active = samples
            .iter()
            .filter(|sample| !sample.stale)
            .copied()
            .collect::<Vec<_>>();
        let mut service = active
            .iter()
            .map(|sample| sample.service_time.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>();
        let mut queue = active
            .iter()
            .map(|sample| sample.queue_wait.as_secs_f64() * 1000.0)
            .collect::<Vec<_>>();
        service.sort_by(f64::total_cmp);
        queue.sort_by(f64::total_cmp);
        let max_backlog = active
            .iter()
            .map(|sample| sample.submitted.saturating_sub(sample.processed))
            .max()
            .unwrap_or_default();
        let stale_rows = samples.iter().filter(|sample| sample.stale).count();
        let stages = active.iter().map(|sample| sample.stages).fold(
            OnlineStageProfile::default(),
            |mut total, stage| {
                total.prepare += stage.prepare;
                total.planet_fit += stage.planet_fit;
                total.local_residual += stage.local_residual;
                total.field_hypothesis += stage.field_hypothesis;
                total.hypothesis_residual += stage.hypothesis_residual;
                total
            },
        );
        let stage_sum = stages.prepare
            + stages.planet_fit
            + stages.local_residual
            + stages.field_hypothesis
            + stages.hypothesis_residual;
        let service_total = active
            .iter()
            .map(|sample| sample.service_time)
            .sum::<std::time::Duration>();
        let stale_service = samples
            .iter()
            .filter(|sample| sample.stale)
            .map(|sample| sample.service_time)
            .sum::<std::time::Duration>();
        let stale_queue = samples
            .iter()
            .filter(|sample| sample.stale)
            .map(|sample| sample.queue_wait)
            .sum::<std::time::Duration>();
        println!(
            "  summary kind={event} active={} stale_rows={} stale_service_ms={:.3} stale_queue_ms={:.3} service_ms[p50/p95/max]={}/{}/{} queue_ms[p50/p95/max]={}/{}/{} stage_total_ms[prepare/planet_fit/local_residual/field_hypothesis/hypothesis_residual/other]={:.3}/{:.3}/{:.3}/{:.3}/{:.3}/{:.3} max_backlog={}",
            active.len(),
            stale_rows,
            stale_service.as_secs_f64() * 1000.0,
            stale_queue.as_secs_f64() * 1000.0,
            percentile_text(&service, 0.50),
            percentile_text(&service, 0.95),
            service
                .last()
                .map_or_else(|| "n/a".to_owned(), |value| format!("{value:.3}")),
            percentile_text(&queue, 0.50),
            percentile_text(&queue, 0.95),
            queue
                .last()
                .map_or_else(|| "n/a".to_owned(), |value| format!("{value:.3}")),
            stages.prepare.as_secs_f64() * 1000.0,
            stages.planet_fit.as_secs_f64() * 1000.0,
            stages.local_residual.as_secs_f64() * 1000.0,
            stages.field_hypothesis.as_secs_f64() * 1000.0,
            stages.hypothesis_residual.as_secs_f64() * 1000.0,
            service_total.saturating_sub(stage_sum).as_secs_f64() * 1000.0,
            max_backlog,
        );
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let position = (sorted.len() - 1) as f64 * quantile;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let fraction = position - lower as f64;
    Some(sorted[lower] + (sorted[upper] - sorted[lower]) * fraction)
}

fn percentile_text(sorted: &[f64], quantile: f64) -> String {
    percentile(sorted, quantile).map_or_else(|| "n/a".to_owned(), |value| format!("{value:.3}"))
}

fn run_oracle_diagnostic(
    events: &[einsteinbot::event_store::StoredEvent],
    options: &Options,
) -> Result<(), String> {
    let mut groups: Vec<(
        Vec<einsteinbot::protocol::Planet>,
        Vec<einsteinbot::event_store::StoredEvent>,
    )> = Vec::new();
    let mut current_planets: Option<Vec<einsteinbot::protocol::Planet>> = None;
    let mut current_events = Vec::new();
    for event in events {
        match &event.event {
            RecordedEvent::Inbound(ServerEvent::Planets { planets }) => {
                if let Some(current) = current_planets.replace(planets.clone()) {
                    groups.push((current, std::mem::take(&mut current_events)));
                }
            }
            _ if current_planets.is_some() => current_events.push(event.clone()),
            _ => {}
        }
    }
    if let Some(planets) = current_planets {
        groups.push((planets, current_events));
    }
    if groups.is_empty() {
        return Err(
            "capture has no recorded Planets event; --oracle needs a sendplanets 1 capture"
                .to_owned(),
        );
    }

    let world = WorldParameters::default();
    println!(
        "capture={} oracle_segments={}",
        options.capture_id,
        groups.len()
    );
    for (index, (planets, segment_events)) in groups.iter().enumerate() {
        let shots = shot_observations(segment_events);
        let map = PlanetMap {
            planets: planets.clone(),
        };
        if shots.is_empty() {
            println!(
                "segment={} planets={} shots=0 oracle_train_rmse=unavailable oracle_validation_rmse=unavailable",
                index + 1,
                planets.len()
            );
            continue;
        }
        let split = split_shots(&shots, options.validation_every);
        let training_rmse = position_rmse(&map, &split.0, world)
            .map(|value| format!("{value:.6}"))
            .unwrap_or_else(|_| "unavailable".to_owned());
        let validation_rmse = position_rmse(&map, &split.1, world)
            .map(|value| format!("{value:.6}"))
            .unwrap_or_else(|_| "unavailable".to_owned());
        println!(
            "segment={} planets={} shots={} oracle_train_rmse={} oracle_validation_rmse={}",
            index + 1,
            planets.len(),
            shots.len(),
            training_rmse,
            validation_rmse
        );
        for ((start_x, start_y), position_shots) in group_shots_by_launch_point(segment_events) {
            let launch_rmse = position_rmse(&map, &position_shots, world)
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|_| "unavailable".to_owned());
            println!(
                "  launch_point=({:.1},{:.1}) shots={} oracle_rmse={}",
                start_x as f64 / 10.0,
                start_y as f64 / 10.0,
                position_shots.len(),
                launch_rmse
            );
            let mut per_shot = position_shots
                .into_iter()
                .filter_map(|shot| {
                    let rmse = shot_position_rmse(&map, &shot, world).ok()??;
                    Some((shot, rmse))
                })
                .collect::<Vec<_>>();
            per_shot.sort_by(|left, right| right.1.total_cmp(&left.1));
            for (shot, rmse) in per_shot.into_iter().take(3) {
                let start = &shot.points[0];
                println!(
                    "    worst_shot start=({:.1},{:.1}) points={} rmse={:.3}",
                    start.x,
                    start.y,
                    shot.points.len(),
                    rmse
                );
            }
        }
        let progress = ProgressBar::new_spinner();
        progress.enable_steady_tick(std::time::Duration::from_millis(100));
        progress.set_message(format!(
            "fitting candidates for oracle segment {}/{}",
            index + 1,
            groups.len()
        ));
        let (training, validation) = split;
        let candidates = fit_greedy_source_counts(
            &training,
            &validation,
            world,
            options.max_sources,
            options.radius_prior,
            options.grid_spacing,
        )
        .map_err(|error| error.to_string())?;
        progress.finish_and_clear();
        for candidate in candidates.candidates {
            println!(
                "  candidate_sources={} train_rmse={:.3} validation_rmse={}",
                candidate.result.map.planets.len(),
                candidate.result.residual_rmse,
                candidate
                    .validation_rmse
                    .map(|value| format!("{value:.3}"))
                    .unwrap_or_else(|| "unavailable".to_owned())
            );
        }
        println!("  search_stopped={:?}", candidates.stop_reason);
    }
    println!("Oracle values score maps and identify map epochs; fits use only shot paths.");
    println!(
        "RMSE is per-coordinate position error in world units; compare held-out scores first."
    );
    Ok(())
}

fn group_shots_by_launch_point(
    events: &[einsteinbot::event_store::StoredEvent],
) -> BTreeMap<(i64, i64), Vec<einsteinbot::simulation::ShotObservation>> {
    let mut groups = BTreeMap::new();
    for stored in events {
        let RecordedEvent::Inbound(event @ ServerEvent::ShotFin { .. }) = &stored.event else {
            continue;
        };
        let Ok(shot) = einsteinbot::simulation::ShotObservation::try_from(event) else {
            continue;
        };
        let Some(start) = shot.points.first() else {
            continue;
        };
        // Bucket f32 protocol coordinates to 0.1 world units to absorb wire rounding.
        let key = (
            (f64::from(start.x) * 10.0).round() as i64,
            (f64::from(start.y) * 10.0).round() as i64,
        );
        groups.entry(key).or_insert_with(Vec::new).push(shot);
    }
    groups
}

fn parse_args<I>(args: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut db_path = PathBuf::from("einsteinbot.sqlite3");
    let mut capture_id = None;
    // Finite-difference fitting cost grows steeply with candidate count. Keep
    // the default practical for an interactive first pass; users can raise it.
    let mut max_sources = 4;
    let mut radius_prior = 30.0_f64;
    let mut grid_spacing = 80.0_f64;
    let mut validation_every = 5;
    let mut use_oracle = true;
    let mut online = false;
    let mut profile_events = false;
    let mut replay_speed = 1.0_f64;
    let mut replay_speed_explicit = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--db-path" => {
                db_path = PathBuf::from(value(&mut args, &arg)?);
            }
            "--capture-id" => {
                capture_id = Some(parse_value(&value(&mut args, &arg)?, &arg)?);
            }
            "--max-sources" => {
                max_sources = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--radius-prior" => {
                radius_prior = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--grid-spacing" => {
                grid_spacing = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--validation-every" => {
                validation_every = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--no-oracle" => use_oracle = false,
            "--online" => online = true,
            "--profile-events" => profile_events = true,
            "--speed" => {
                replay_speed = parse_value(&value(&mut args, &arg)?, &arg)?;
                replay_speed_explicit = true;
            }
            _ => return Err(format!("unknown option {arg}\n\n{}", help_text())),
        }
    }
    let capture_id = capture_id.ok_or_else(|| "--capture-id is required".to_owned())?;
    if max_sources == 0 || max_sources > 64 {
        return Err("--max-sources must be between 1 and 64".to_owned());
    }
    if !radius_prior.is_finite() || radius_prior <= 0.0 {
        return Err("--radius-prior must be finite and positive".to_owned());
    }
    if !grid_spacing.is_finite() || grid_spacing <= 0.0 {
        return Err("--grid-spacing must be finite and positive".to_owned());
    }
    if validation_every == 1 {
        return Err("--validation-every must be 0 or at least 2".to_owned());
    }
    if !online && (profile_events || replay_speed_explicit) {
        return Err("--speed and --profile-events require --online".to_owned());
    }
    if !replay_speed.is_finite() || replay_speed < 0.0 {
        return Err("--speed must be finite and nonnegative".to_owned());
    }
    Ok(Some(Options {
        db_path,
        capture_id,
        max_sources,
        radius_prior,
        grid_spacing,
        validation_every,
        use_oracle,
        online,
        profile_events,
        replay_speed,
    }))
}

fn split_shots(
    shots: &[einsteinbot::simulation::ShotObservation],
    validation_every: usize,
) -> (
    Vec<einsteinbot::simulation::ShotObservation>,
    Vec<einsteinbot::simulation::ShotObservation>,
) {
    if validation_every == 0 || shots.len() < validation_every {
        return (shots.to_vec(), Vec::new());
    }
    let mut training = Vec::new();
    let mut validation = Vec::new();
    for (index, shot) in shots.iter().enumerate() {
        if (index + 1) % validation_every == 0 {
            validation.push(shot.clone());
        } else {
            training.push(shot.clone());
        }
    }
    if training.is_empty() || validation.is_empty() {
        (shots.to_vec(), Vec::new())
    } else {
        (training, validation)
    }
}

fn value<I>(args: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("missing value for {option}"))
}

fn parse_value<T>(value: &str, option: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| format!("invalid {option} value {value:?}: {error}"))
}

fn print_help() {
    print!("{}", help_text());
}

fn help_text() -> &'static str {
    "Usage: estimate --capture-id ID [OPTIONS]\n\
     \n\
     Options:\n\
       --db-path PATH          SQLite file (default: einsteinbot.sqlite3)\n\
       --capture-id ID         Capture to analyze (required)\n\
       --max-sources N         Maximum candidate planet count (default: 4)\n\
       --radius-prior VALUE    Radius used for seeding (default: 30)\n\
       --grid-spacing VALUE    Deterministic seed-grid spacing (default: 80)\n\
       --validation-every N    Hold out every Nth shot; 0 disables (default: 5)\n\
       --no-oracle             Ignore captured Planets maps\n\
       --online                Replay through the live map worker and report timing\n\
       --speed FACTOR          Timing factor: 1 uses recorded intervals, 0 is unpaced\n\
       --profile-events        Print timing for every map event (default: ShotFin only)\n\
       -h, --help              Show this help\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use einsteinbot::protocol::{Point, ServerEvent};

    fn shot(sequence: i64, start_x: f32, start_y: f32) -> einsteinbot::event_store::StoredEvent {
        einsteinbot::event_store::StoredEvent {
            sequence,
            timestamp_unix_ms: sequence,
            bot_name: None,
            event: RecordedEvent::Inbound(ServerEvent::ShotFin {
                player_id: 0,
                angle: 0.0,
                velocity: 2.0,
                points: vec![
                    Point {
                        x: start_x,
                        y: start_y,
                    },
                    Point {
                        x: start_x + 1.0,
                        y: start_y,
                    },
                    Point {
                        x: start_x + 2.0,
                        y: start_y,
                    },
                ],
            }),
        }
    }

    #[test]
    fn oracle_scores_group_shots_by_launch_point_not_finish_order() {
        let events = vec![
            shot(1, 10.0, 20.0),
            shot(2, 100.0, 200.0),
            // This old trajectory finished after the shot from the new position.
            shot(3, 10.0, 20.0),
        ];
        let groups = group_shots_by_launch_point(&events);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[&(100, 200)].len(), 2);
        assert_eq!(groups[&(1000, 2000)].len(), 1);
    }

    #[test]
    fn online_profile_replays_map_events_and_marks_reset_seeds() {
        let event = |sequence, timestamp_unix_ms, event| einsteinbot::event_store::StoredEvent {
            sequence,
            timestamp_unix_ms,
            bot_name: None,
            event: RecordedEvent::Inbound(event),
        };
        let events = vec![
            event(
                1,
                1000,
                ServerEvent::PlayerPosition {
                    player_id: 0,
                    x: 100.0,
                    y: 100.0,
                },
            ),
            event(
                2,
                1001,
                ServerEvent::OwnEnergy {
                    player_id: 0,
                    energy: 8.0,
                },
            ),
            event(3, 1002, ServerEvent::Planets { planets: vec![] }),
        ];

        let samples = replay_capture(&events, 0.0).unwrap();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].event, "player_position");
        assert_eq!(samples[1].event, "reset_seed_player_position");
        assert_eq!(samples[1].epoch, 1);
    }

    #[test]
    fn profile_percentiles_interpolate_and_report_empty_samples() {
        assert_eq!(percentile(&[], 0.50), None);
        assert_eq!(percentile(&[7.0], 0.95), Some(7.0));
        assert_eq!(percentile(&[0.0, 10.0], 0.50), Some(5.0));
    }

    #[test]
    fn online_profile_rejects_invalid_speed_before_starting_a_worker() {
        assert!(replay_capture(&[], -1.0).is_err());
    }
}
