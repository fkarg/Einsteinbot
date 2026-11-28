//! `devtool schedule`: the estimator under live timing (formerly the `SCHEDULE` mode of the
//! `diagnose_replay` test). A path arrives every `--period-ms` of wall clock; in between the
//! estimator takes slices as the live map's worker thread does. Reports the publish lag (path
//! arrival to its map being published) and the recall timeline. Not deterministic: the wall
//! clock cuts the slices.
use crate::config::Config;
use crate::world;
use einsteinbot::estimator::Estimator;
use serde_json::json;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

pub fn run(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let (mut spec, mut config_path, mut db) =
        (String::new(), None, "einsteinbot.sqlite3".to_owned());
    let (mut paths, mut period_ms, mut threads) = (60usize, 1000u64, 0usize);
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--world" => spec = value()?,
            "--config" => config_path = Some(value()?),
            "--db" => db = value()?,
            "--paths" => paths = value()?.parse().map_err(|e| format!("{e}"))?,
            "--period-ms" => period_ms = value()?.parse().map_err(|e| format!("{e}"))?,
            "--threads" => threads = value()?.parse().map_err(|e| format!("{e}"))?,
            _ => {
                println!(
                    "Usage: devtool schedule --world SPEC [--config cfg.json] [--paths N] [--period-ms MS (1000)] [--threads N (all cores)] [--db PATH]\nJSON lines: one per path (publish lag, planets, recall within 2 units) and a summary (worst/p90/median lag)."
                );
                return Ok(());
            }
        }
    }
    let config = Config::load(config_path.as_deref())?;
    let world = world::load(&spec, paths, &db)?;
    let mut settings = config.estimator.clone();
    if threads > 0 {
        settings.threads = threads;
    }
    let mut estimator = Estimator::new(settings);
    let never = AtomicBool::new(false);
    let shots = world.paths();
    let total = shots.len().min(paths);
    let period = Duration::from_millis(period_ms);
    let recall = |e: &Estimator| {
        let planets = e.belief().map_or(vec![], |b| b.map.planets.clone());
        let hit = world
            .truth
            .iter()
            .filter(|t| planets.iter().any(|p| (p.x - t.x).hypot(p.y - t.y) <= 2.0))
            .count();
        (planets.len(), hit)
    };
    let t0 = Instant::now();
    let mut lags = Vec::with_capacity(total);
    for (n, (_, obs)) in shots.iter().take(total).enumerate() {
        let arrival = t0 + period * n as u32;
        while Instant::now() < arrival {
            if estimator.search_pending() {
                estimator.step(&never);
            } else {
                std::thread::sleep(arrival.saturating_duration_since(Instant::now()));
            }
        }
        let event = einsteinbot::protocol::ServerEvent::ShotFin {
            player_id: obs.player_id,
            angle: obs.angle_degrees,
            velocity: obs.velocity,
            points: obs.points.clone(),
        };
        estimator.observe(&event, &never);
        let lag = arrival.elapsed().as_secs_f64() * 1e3;
        lags.push(lag);
        let (k, hit) = recall(&estimator);
        println!(
            "{}",
            json!({"path": n + 1, "publish_lag_ms": lag, "t_ms": t0.elapsed().as_secs_f64() * 1e3, "k": k, "recall2": hit})
        );
    }
    lags.sort_by(f64::total_cmp);
    let q = |q: f64| {
        lags.get(((lags.len() as f64 * q).ceil() as usize).saturating_sub(1))
            .copied()
    };
    let (k, hit) = recall(&estimator);
    println!(
        "{}",
        json!({"world": world.id, "paths": total, "period_ms": period_ms, "worst_publish_lag_ms": q(1.0),
               "p90_lag_ms": q(0.9), "median_lag_ms": q(0.5), "k": k, "recall2": hit, "k_true": world.truth.len()})
    );
    Ok(())
}
