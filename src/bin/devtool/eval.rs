//! `devtool eval`: one configuration on one world at one fidelity (paths), deterministic, as a
//! metrics JSON; cached by (revision, config hash, world, fidelity, mode).
use crate::config::{Config, fnv1a};
use crate::targeting::{self, BeliefRecord, Outcome, par_map, score_once, situations, summary};
use crate::world::{self, World};
use einsteinbot::estimator::{Estimator, SearchLimit, exact_chi2, shot_position_rmse};
use einsteinbot::field_score::score_field;
use einsteinbot::map::PlanetMap;
use einsteinbot::protocol::{Planet, ServerEvent};
use einsteinbot::simulation::{ShotObservation, WorldParameters};
use einsteinbot::strategy::{MapBelief, TargetingPolicy};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const USAGE: &str = "Usage: devtool eval --world synth:SEED[:K] | w1:SEED[:DISTANCE] | capture:ID[:MAP] [--config cfg.json] [--paths N] [--mode estimator|targeting|both] [--belief recorded|refit] [--situations N] [--checkpoints 4,8,16,32,60] [--threads N] [--db PATH] [--cache DIR | --no-cache] [--out metrics.json]
Deterministic: slices are never cut by the wall clock (the config's slice_ceiling_ms is ignored) and no audit runs.
Estimator metrics at each checkpoint (paths): recall within 2 and 15 units, spurious planets, chi2 ratio to the truth on the exact likelihood, field RMSE explored/unexplored/whole (flat in *_flat), prequential path RMSE; work counters.
Targeting: captures replay the recorded decisions (beliefs as recorded or refit by this config's estimator; wasted = victim killed first); synthetic worlds score --situations generated decisions at each checkpoint. Shots are scored on the true map.";

pub struct Args {
    pub world: String,
    pub config: Option<String>,
    pub paths: usize,
    pub mode: String,
    pub belief: String,
    pub situations: usize,
    pub checkpoints: Vec<usize>,
    pub threads: usize,
    pub db: String,
    pub cache: Option<PathBuf>,
    pub out: Option<String>,
}

pub fn parse(mut args: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut a = Args {
        world: String::new(),
        config: None,
        paths: 60,
        mode: "both".into(),
        belief: "refit".into(),
        situations: 16,
        checkpoints: vec![4, 8, 16, 32, 60],
        threads: 1,
        db: "einsteinbot.sqlite3".into(),
        cache: Some(PathBuf::from("target/devtool-cache")),
        out: None,
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        let num = |v: String| v.parse::<usize>().map_err(|e| format!("{v}: {e}"));
        match arg.as_str() {
            "--world" => a.world = value()?,
            "--config" => a.config = Some(value()?),
            "--paths" => a.paths = num(value()?)?,
            "--mode" => a.mode = value()?,
            "--belief" => a.belief = value()?,
            "--situations" => a.situations = num(value()?)?,
            "--checkpoints" => {
                a.checkpoints = value()?
                    .split(',')
                    .map(|v| num(v.to_owned()))
                    .collect::<Result<_, _>>()?
            }
            "--threads" => a.threads = num(value()?)?.max(1),
            "--db" => a.db = value()?,
            "--cache" => a.cache = Some(PathBuf::from(value()?)),
            "--no-cache" => a.cache = None,
            "--out" => a.out = Some(value()?),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other}\n{USAGE}")),
        }
    }
    if a.world.is_empty() {
        return Err(format!("--world is required\n{USAGE}"));
    }
    if !["estimator", "targeting", "both"].contains(&a.mode.as_str()) {
        return Err("--mode must be estimator, targeting or both".into());
    }
    if !["recorded", "refit"].contains(&a.belief.as_str()) {
        return Err("--belief must be recorded or refit".into());
    }
    Ok(Some(a))
}

pub fn revision() -> (String, bool) {
    (
        option_env!("EINSTEINBOT_GIT_REVISION")
            .unwrap_or("unknown")
            .to_owned(),
        option_env!("EINSTEINBOT_GIT_DIRTY").is_none_or(|d| d == "true"),
    )
}

pub fn run(args: Args) -> Result<(), String> {
    let config = Config::load(args.config.as_deref())?;
    let (revision, dirty) = revision();
    let key = format!(
        "{revision}|{}|{}|paths={}|mode={}|belief={}|situations={}|checkpoints={:?}",
        config.hash(),
        args.world,
        args.paths,
        args.mode,
        args.belief,
        args.situations,
        args.checkpoints
    );
    let cache_file = args
        .cache
        .as_ref()
        .filter(|_| !dirty)
        .map(|dir| dir.join(format!("{:016x}.json", fnv1a(key.as_bytes()))));
    if let Some(file) = &cache_file
        && let Ok(text) = std::fs::read_to_string(file)
        && serde_json::from_str::<Value>(&text).is_ok_and(|v| v["cache_key"] == json!(key))
    {
        return emit(&args, &text);
    }
    if dirty && args.cache.is_some() {
        eprintln!("devtool eval: uncommitted source, result not cached");
    }
    let world = world::load(&args.world, args.paths, &args.db)?;
    let metrics = evaluate(&world, &config, &args);
    let mut out = json!({
        "cache_key": key,
        "revision": revision,
        "source_changes": dirty,
        "config_hash": config.hash(),
        "config": serde_json::from_str::<Value>(&config.canonical()).unwrap(),
        "world": world.id,
        "fidelity": {"paths": args.paths},
        "mode": args.mode,
    });
    for (k, v) in metrics.as_object().unwrap() {
        out[k] = v.clone();
    }
    let text = serde_json::to_string_pretty(&out).unwrap();
    if let Some(file) = &cache_file {
        std::fs::create_dir_all(file.parent().unwrap()).map_err(|e| e.to_string())?;
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, &text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, file).map_err(|e| e.to_string())?;
    }
    emit(&args, &text)
}

fn emit(args: &Args, text: &str) -> Result<(), String> {
    match &args.out {
        Some(path) => std::fs::write(path, text).map_err(|e| format!("{path}: {e}")),
        None => {
            println!("{text}");
            Ok(())
        }
    }
}

fn cpu_seconds() -> f64 {
    // SAFETY: getrusage only writes the struct it is given.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        let t = |v: libc::timeval| v.tv_sec as f64 + v.tv_usec as f64 * 1e-6;
        t(usage.ru_utime) + t(usage.ru_stime)
    }
}

fn distance(a: &Planet, b: &Planet) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

/// Recall, spurious, chi2 ratio and field errors of `planets` after the paths `obs`.
fn score_map(world: &World, planets: &[Planet], obs: &[ShotObservation]) -> Value {
    let truth = &world.truth;
    let found = |t: &Planet, limit: f64| planets.iter().any(|p| distance(p, t) <= limit);
    let recall = |limit: f64| truth.iter().filter(|t| found(t, limit)).count();
    let spurious = planets
        .iter()
        .filter(|p| !truth.iter().any(|t| distance(p, t) <= 15.0))
        .count();
    let chi2 = |m: &[Planet]| exact_chi2(obs, m).map(|(c, _)| c);
    let ratio = chi2(planets).zip(chi2(truth)).map(|(a, b)| a / b);
    let truth_map = PlanetMap {
        planets: truth.clone(),
    };
    let w = WorldParameters::default();
    let belief = MapBelief::new(
        PlanetMap {
            planets: planets.to_vec(),
        },
        None,
    );
    let flat = MapBelief::new(PlanetMap { planets: vec![] }, None);
    let field = |b: &MapBelief| score_field(&truth_map, b, obs, w);
    let rmse =
        |s: &Option<einsteinbot::field_score::FieldStats>| s.as_ref().and_then(|s| s.vector_rmse);
    let (f, f0) = (field(&belief), field(&flat));
    json!({
        "k_true": truth.len(),
        "k": planets.len(),
        "recall2": recall(2.0),
        "recall15": recall(15.0),
        "spurious": spurious,
        "intended_recall15": world.intended.iter().filter(|&&i| found(&truth[i], 15.0)).count(),
        "chi2_ratio": ratio,
        "field_explored_rmse": f.as_ref().and_then(|f| rmse(&f.explored)),
        "field_unexplored_rmse": f.as_ref().and_then(|f| rmse(&f.unexplored)),
        "field_whole_rmse": f.as_ref().and_then(|f| rmse(&f.whole)),
        "field_explored_rmse_flat": f0.as_ref().and_then(|f| rmse(&f.explored)),
        "field_unexplored_rmse_flat": f0.as_ref().and_then(|f| rmse(&f.unexplored)),
        "field_whole_rmse_flat": f0.as_ref().and_then(|f| rmse(&f.whole)),
    })
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    v.sort_by(f64::total_cmp);
    (!v.is_empty()).then(|| v[v.len() / 2])
}

/// What the estimator replay leaves for scoring.
struct Replay {
    /// Belief after each distinct path (index = paths - 1).
    after_path: Vec<BeliefRecord>,
    /// Event index of each distinct path.
    path_event: Vec<usize>,
    checkpoints: Vec<Value>,
    work: Value,
}

/// The estimator on the world's events up to `paths` distinct paths, settled after every path.
fn replay(world: &World, config: &Config, args: &Args) -> Replay {
    let mut settings = config.estimator.clone();
    settings.slice_ceiling = None;
    settings.audit_enabled = false;
    settings.threads = args.threads;
    let mut estimator = Estimator::new(settings);
    let never = AtomicBool::new(false);
    let paths = world.paths();
    let last_needed = args.paths.min(paths.len());
    let mut cps: Vec<usize> = args
        .checkpoints
        .iter()
        .copied()
        .filter(|&n| n < args.paths.min(paths.len()))
        .collect();
    cps.push(args.paths.min(paths.len()));
    let world_params = WorldParameters::default();
    let (cpu0, wall0) = (cpu_seconds(), Instant::now());
    let mut out = Replay {
        after_path: vec![],
        path_event: vec![],
        checkpoints: vec![],
        work: Value::Null,
    };
    let (mut evaluations, mut slowest_ms, mut violations, mut time_cuts) = (0u64, 0.0f64, 0, 0);
    let (mut pred, mut pred_flat): (Vec<f64>, Vec<f64>) = (vec![], vec![]);
    let flat = PlanetMap { planets: vec![] };
    let mut observed: Vec<ShotObservation> = Vec::new();
    for (i, ev) in world.events.iter().enumerate() {
        if out.after_path.len() >= last_needed {
            break;
        }
        if i < world.start
            && !matches!(
                ev.event,
                ServerEvent::PlayerPosition { .. } | ServerEvent::PlayerLeave { .. }
            )
        {
            continue;
        }
        let next = out.after_path.len();
        let is_new_path = paths.get(next).is_some_and(|(j, _)| *j == i);
        if !is_new_path {
            if !matches!(ev.event, ServerEvent::ShotFin { .. }) {
                estimator.observe(&ev.event, &never);
            }
            continue;
        }
        let obs = &paths[next].1;
        // Prequential: the new path against the map published before it.
        if next < args.paths {
            let map = estimator.belief().map_or(flat.clone(), |b| b.map.clone());
            if let (Ok(Some(a)), Ok(Some(b))) = (
                shot_position_rmse(&map, obs, world_params),
                shot_position_rmse(&flat, obs, world_params),
            ) {
                pred.push(a);
                pred_flat.push(b);
            }
        }
        let started = Instant::now();
        estimator.observe(&ev.event, &never);
        estimator.settle(&never);
        let ms = started.elapsed().as_secs_f64() * 1e3;
        observed.push(obs.clone());
        if let Some(r) = estimator.last_report() {
            evaluations += r.evaluations;
            violations += usize::from(!r.violations.is_empty());
        }
        let belief = estimator.belief();
        time_cuts +=
            usize::from(belief.is_some_and(|b| b.search_limit == Some(SearchLimit::TimeBudget)));
        slowest_ms = slowest_ms.max(ms);
        let record = BeliefRecord {
            planets: belief.map_or(vec![], |b| b.map.planets.clone()),
            model: belief.and_then(|b| b.residual_model),
        };
        let n = observed.len();
        if cps.contains(&n) {
            let mut row = score_map(world, &record.planets, &observed);
            row["paths"] = json!(n);
            row["evaluations"] = json!(evaluations);
            row["cpu_s"] = json!(cpu_seconds() - cpu0);
            row["pred_rmse_median"] = json!(median(pred.clone()));
            row["pred_rmse_flat_median"] = json!(median(pred_flat.clone()));
            row["search_limit"] = json!(belief.and_then(|b| b.search_limit));
            row["count_limit_detected_at"] =
                json!(belief.and_then(|b| b.count_limit.as_ref()?.detected_at_path));
            out.checkpoints.push(row);
        }
        out.after_path.push(record);
        out.path_event.push(i);
    }
    let n = out.after_path.len().min(args.paths).max(1);
    out.work = json!({
        "paths": out.after_path.len(),
        "evaluations": evaluations,
        "evaluations_per_path": evaluations as f64 / n as f64,
        "cpu_s": cpu_seconds() - cpu0,
        "wall_s": wall0.elapsed().as_secs_f64(),
        "slowest_update_ms": slowest_ms,
        "ceiling_violations": violations,
        "time_budget_cuts": time_cuts,
        "threads": args.threads,
    });
    out
}

fn evaluate(world: &World, config: &Config, args: &Args) -> Value {
    let wants_targeting = args.mode != "estimator";
    let refit_decisions = wants_targeting && world.is_capture() && args.belief == "refit";
    let needs_estimator =
        args.mode != "targeting" || refit_decisions || (wants_targeting && !world.is_capture());
    let replay = needs_estimator.then(|| replay(world, config, args));
    let mut out = json!({});
    if let Some(r) = &replay
        && args.mode != "targeting"
    {
        out["estimator"] = json!({"checkpoints": r.checkpoints, "work": r.work});
    }
    if !wants_targeting {
        return out;
    }
    if world.is_capture() {
        // Decisions before the (paths + 1)-th distinct path.
        let paths = world.paths();
        let limit_seq = paths
            .get(args.paths)
            .map_or(i64::MAX, |(i, _)| world.events[*i].seq);
        let beliefs: Vec<Option<BeliefRecord>> = world
            .decisions
            .iter()
            .map(|d| {
                if d.seq >= limit_seq {
                    return None;
                }
                match &replay {
                    None => Some(BeliefRecord::recorded(&d.data)),
                    Some(r) => {
                        let n = r
                            .path_event
                            .iter()
                            .filter(|&&i| world.events[i].seq < d.seq)
                            .count();
                        Some(n.checked_sub(1).map_or(
                            BeliefRecord {
                                planets: vec![],
                                model: None,
                            },
                            |k| r.after_path[k].clone(),
                        ))
                    }
                }
            })
            .collect();
        let outcomes = targeting::replay_capture(world, config.targeting, &beliefs, args.threads);
        out["targeting"] = summary(&outcomes);
        out["targeting"]["belief"] = json!(args.belief);
    } else {
        let r = replay
            .as_ref()
            .expect("synthetic targeting needs the estimator");
        let truth = PlanetMap {
            planets: world.truth.clone(),
        };
        let seed = fnv1a(world.id.as_bytes());
        let set = situations(seed, &world.truth, args.situations);
        let policy = TargetingPolicy::new(config.targeting);
        let mut all: Vec<Outcome> = Vec::new();
        let mut rows = Vec::new();
        let paths = world.paths();
        for cp in r
            .checkpoints
            .iter()
            .map(|c| c["paths"].as_u64().unwrap() as usize)
        {
            let history: Vec<ShotObservation> =
                paths[..cp].iter().map(|(_, o)| o.clone()).collect();
            let belief = r.after_path[cp - 1].belief(&history);
            let outcomes: Vec<Outcome> = par_map(set.len(), args.threads, |i| {
                let mut state = set[i].state(&history);
                state.set_map_belief(Some(belief.clone()));
                score_once(&policy, &truth, &state, set[i].energy)
            });
            let mut row = summary(&outcomes);
            row["paths"] = json!(cp);
            rows.push(row);
            all.extend(outcomes);
        }
        out["targeting"] = summary(&all);
        out["targeting"]["checkpoints"] = json!(rows);
    }
    out
}
