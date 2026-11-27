//! P6: warm-started incremental updates ("B") against from-scratch rebuilds
//! (`docs/estimator-experiments.md`, "P6 results"). Paths arrive one by one in a fixed order. An
//! update keeps the current map; refits it on a bounded thinned row set of all paths so far at
//! relative sd levels, grows planets (births on the same rows) and prunes (deaths). Audits
//! rebuild from scratch (E9 cascade, then the E9c relative polish with a guarded exact refit) and
//! compare. Truth scores only. Output lines are `TAG key=value ...` for `scripts/p6_tables.py`.
//!
//! Run (release): `cargo test --release --lib diagnose_incremental -- --ignored --nocapture
//! --test-threads=1`. Environment: `SEEDS`, `KS`, `LIVE` (as `cases`), `PATHS` (default 60),
//! `CONFIGS` (space separated, see `Config::parse`), `CHECKPOINTS` (default 4,8,16,32,60), `CACHE`
//! (rebuild cache directory, default /tmp/p6-cache), `THREADS`, `PROD=1` (also run the current
//! production update path by path), `PLANETS=1` (per-planet support lines at the checkpoints),
//! `NOISE`. `diagnose_rebuild` fills the cache and prints the ceiling lines (`COUNTS`).
use super::polish::{displacement, field_rmse};
use super::thinning::{grow, parse, prune, score, search_shots, thin_shots};
use super::*;
use crate::estimator::fit::{Budget, planet_at};
use crate::estimator::search::{Mode, refit, same_map, search, seeds};
use crate::estimator::work::{TEST_THREADS, cores};
use std::borrow::Cow;

const CELL: f64 = 14.0;
const PER_CELL: usize = 2;
const CASCADE: &str =
    "thinrel:14:2:500:strong:0.1+thinrel:14:2:1000:strong:0.02+thinrel:14:2:2000:strong:0.005";

/// `lv=0.02/0.005:cap=3000:d=1:ev=3000:rf=1500:audit=16` (name is the whole string). `lv` relative
/// sd levels run in order each update, `cap` rows kept by the thinning rule (0: all rows, no
/// thinning), `d` depth of the death descent, `ev` evaluation cap of the births of a level, `rf`
/// evaluation cap of the warm refit of a level, `dv` evaluation cap of the deaths of a level, `dn`
/// `lr` radius of the local warm refit, `dl` threshold of the local death test, run deaths every dn-th update (and after any birth), `audit=N` rebuild every N paths and adopt it when
/// it fits better, `probe=N` the same without adopting.
struct Config {
    name: String,
    levels: Vec<f64>,
    cap: usize,
    depth: usize,
    births: u64,
    refit: u64,
    /// Evaluation cap of the death trials of a level (default: the births cap).
    deaths: Option<u64>,
    /// Deaths run only every this many updates (default 1), and always right after a birth.
    deaths_every: usize,
    /// Warm refit only the planets within this distance of the new path's rows, the others held
    /// fixed (0: refit all).
    local: f64,
    /// Local death test instead of the evidence descent: the weakest planets are removed, their
    /// neighbours (within `local`, at least 100) refit, and the removal is kept when the chi2 at
    /// the level rises by less than this.
    local_death: Option<f64>,
    /// (cadence, adopt)
    audit: Option<(usize, bool)>,
}

impl Config {
    fn parse(spec: &str) -> Config {
        let mut c = Config {
            name: spec.to_string(),
            levels: vec![0.005],
            cap: 3000,
            depth: 1,
            births: 3000,
            refit: 1500,
            deaths: None,
            deaths_every: 1,
            local: 0.0,
            local_death: None,
            audit: None,
        };
        for kv in spec.split(':') {
            let (k, v) = kv.split_once('=').unwrap();
            match k {
                "lv" => c.levels = v.split('/').map(|x| x.parse().unwrap()).collect(),
                "cap" => c.cap = v.parse().unwrap(),
                "d" => c.depth = v.parse().unwrap(),
                "ev" => c.births = v.parse().unwrap(),
                "rf" => c.refit = v.parse().unwrap(),
                "dv" => c.deaths = Some(v.parse().unwrap()),
                "dl" => c.local_death = Some(v.parse().unwrap()),
                "lr" => c.local = v.parse().unwrap(),
                "dn" => c.deaths_every = v.parse().unwrap(),
                "audit" => c.audit = Some((v.parse().unwrap(), true)),
                "probe" => c.audit = Some((v.parse().unwrap(), false)),
                other => panic!("unknown key {other}"),
            }
        }
        c
    }
}

fn env_list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name)
        .unwrap_or_else(|_| default.into())
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect()
}

fn cache_dir() -> String {
    std::env::var("CACHE").unwrap_or_else(|_| "/tmp/p6-cache".into())
}

pub(super) fn world_id(case: &Case) -> String {
    // Without the path count, so the id is the same whatever PATHS was.
    let name = case.name.split(" paths=").next().unwrap();
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn rows_of(shots: &[Shot]) -> usize {
    shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .map(|p| 2 * p.row_count())
        .sum()
}

fn nodes_of(shots: &[Shot]) -> usize {
    shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .map(|p| p.nodes.len())
        .sum()
}

fn relative_shot(obs: &ShotObservation, level: f64, noise: f64) -> Shot {
    Shot {
        obs: obs.clone(),
        contact: None,
        path: PathData::new_relative(
            &obs.points,
            obs.angle_degrees,
            obs.velocity,
            WorldParameters::default(),
            &[noise],
            level,
        ),
    }
}

fn limits<'a>(max: Option<u64>, ceilings: &'a PartCeilings, seconds: u64) -> Limits<'a> {
    Limits {
        max_planets: 32,
        deadline: Instant::now() + Duration::from_secs(seconds),
        max_evaluations: max,
        parallel_evaluations: true,
        ceilings,
    }
}

pub(super) struct Metrics {
    pub(super) text: String,
    k: usize,
    rec2: usize,
    spur: usize,
    ratio: f64,
    explored: f64,
}

/// Quality of `theta` against the exact likelihood and the truth on the first `n` paths.
pub(super) fn quality(case: &Case, n: usize, theta: &[[f64; 3]]) -> Metrics {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let shots = &case.shots[..n];
    let rows = rows_of(shots);
    let truth = theta_of_planets(&case.truth);
    let exact = Fit::new(shots, world, &never, false, vec![]);
    let s = score(theta, &truth, &exact, rows);
    let (fe, fu, fw) = field_rmse(shots, theta, &truth);
    let (fe0, fu0, fw0) = field_rmse(shots, &[], &truth);
    Metrics {
        text: format!(
            "kt={} k={} rec2={} rec15={} spur={} ratio={:.4e} fe={fe:.4e} fu={fu:.4e} fw={fw:.4e} fe0={fe0:.4e} fu0={fu0:.4e} fw0={fw0:.4e}",
            truth.len(),
            s.k,
            s.recall2,
            s.recall15,
            s.spurious,
            s.ratio
        ),
        k: s.k,
        rec2: s.recall2,
        spur: s.spurious,
        ratio: s.ratio,
        explored: fe,
    }
}

pub(super) struct Rebuilt {
    pub(super) theta: Vec<[f64; 3]>,
    pub(super) evals: u64,
    pub(super) equiv: f64,
    pub(super) wall: f64,
}

/// From scratch on the first `n` paths: the E9 cascade (cap 28000 evaluations) and the E9c
/// program `0.005gd+0.001gd+0` with the guarded exact refit (`G3C1.5`). Cached on disk.
pub(super) fn rebuild(case: &Case, n: usize, noise: f64) -> Rebuilt {
    let path = format!("{}/{}_{n}.txt", cache_dir(), world_id(case));
    if let Ok(text) = std::fs::read_to_string(&path) {
        let mut lines = text.lines();
        let head: Vec<f64> = lines
            .next()
            .unwrap()
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let theta = lines
            .map(|l| {
                let v: Vec<f64> = l.split_whitespace().map(|v| v.parse().unwrap()).collect();
                [v[0], v[1], v[2]]
            })
            .collect();
        return Rebuilt {
            theta,
            evals: head[0] as u64,
            equiv: head[1],
            wall: head[2],
        };
    }
    let started = Instant::now();
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let sub = Case {
        name: case.name.clone(),
        truth: case.truth.clone(),
        shots: case.shots[..n].to_vec(),
    };
    let total_nodes = nodes_of(&sub.shots);
    let ceilings = PartCeilings::default();
    let stats = Stats::default();
    let l = limits(Some(28_000), &ceilings, 36000);
    let run = Run {
        limits: &l,
        stats: &stats,
    };
    let mut equiv = 0.0;
    let mut smodes: Vec<Mode> = Vec::new();
    for (i, stage) in CASCADE.split('+').map(parse).enumerate() {
        let shots = search_shots(&sub, stage, noise, None);
        let cost = nodes_of(&shots);
        let before = stats.evaluations();
        let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats);
        if i == 0 {
            smodes = vec![mode_at(&joint, &[]).unwrap()];
        } else {
            let theta = theta_of_planets(&best(&smodes).planets);
            let b = Budget {
                evaluations: u64::MAX,
                deadline: None,
                threads: cores(),
            };
            smodes = vec![mode_at(&joint, &theta).unwrap()];
            smodes.extend(refit(&joint, &theta, &b));
        }
        grow(&joint, &shots, &mut smodes, &run, world);
        equiv += (stats.evaluations() - before) as f64 * cost as f64 / total_nodes as f64;
    }
    let mut evals = stats.evaluations();
    let mut theta = theta_of_planets(&best(&smodes).planets);
    // Polish: relative levels with growth and deaths on full rows, then a guarded exact refit.
    let budget = if n >= 32 { 12_000 } else { 8_000 };
    let pstats = Stats::default();
    let pl = limits(Some(budget), &ceilings, 36000);
    let prun = Run {
        limits: &pl,
        stats: &pstats,
    };
    let exact = Fit::new(&sub.shots, world, &never, false, vec![]);
    let rows = rows_of(&sub.shots);
    for level in [0.005, 0.001, 0.0] {
        let shots = if level > 0.0 {
            Cow::Owned(search_shots(
                &sub,
                super::thinning::Variant {
                    thin: None,
                    relative: level,
                    resid: false,
                    frozen: false,
                },
                noise,
                None,
            ))
        } else {
            Cow::Borrowed(&sub.shots[..])
        };
        let before = pstats.evaluations();
        let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&pstats);
        let left = budget.saturating_sub(pstats.evaluations());
        let b = Budget {
            evaluations: left.max(500),
            deadline: None,
            threads: cores(),
        };
        let mut modes = vec![mode_at(&joint, &theta).unwrap()];
        modes.extend(refit(&joint, &theta, &b));
        if level > 0.0 {
            grow(&joint, &shots, &mut modes, &prun, world);
            prune(&joint, &mut modes, &prun, 6);
        }
        equiv += (pstats.evaluations() - before) as f64;
        let result = theta_of_planets(&best(&modes).planets);
        if level > 0.0 {
            theta = result;
        } else {
            let (b4, after) = (exact.chi2(&theta).unwrap(), exact.chi2(&result).unwrap());
            if after < b4 && (displacement(&theta, &result) < 3.0 || after / rows as f64 <= 1.5) {
                theta = result;
            }
        }
    }
    evals += pstats.evaluations();
    let wall = started.elapsed().as_secs_f64();
    let mut text = format!("{evals} {equiv} {wall}\n");
    for t in &theta {
        text += &format!("{} {} {}\n", t[0], t[1], t[2]);
    }
    std::fs::create_dir_all(cache_dir()).unwrap();
    // Atomic, so concurrent runs sharing the cache never read a half-written file.
    let tmp = format!("{path}.{}.tmp", std::process::id());
    std::fs::write(&tmp, text).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    Rebuilt {
        theta,
        evals,
        equiv,
        wall,
    }
}

fn distance(a: &[f64; 3], b: &[f64; 3]) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn nearest_distance(p: &[f64; 3], set: &[[f64; 3]]) -> f64 {
    set.iter()
        .map(|q| distance(p, q))
        .fold(f64::INFINITY, f64::min)
}

struct Replay<'a> {
    case: &'a Case,
    noise: f64,
    /// Per relative level, the full-row shots of every path (built up front).
    full: Vec<(f64, Vec<Shot>)>,
}

impl Replay<'_> {
    fn list(&self, level: f64) -> &[Shot] {
        &self.full.iter().find(|(l, _)| *l == level).unwrap().1
    }
}

struct UpdateCost {
    /// Milliseconds in thinning and setup, the warm refit, births, deaths.
    phases_ms: [f64; 4],
    wall_ms: f64,
    evals: u64,
    equiv: f64,
}

/// Local death test: the two planets whose removal raises chi2 least are removed in turn, the
/// planets within `radius` of the removed one refit with the rest fixed, and the removal is kept
/// when the chi2 rises by less than `threshold`.
fn local_deaths(
    joint: &Fit,
    shots: &[Shot],
    theta: &mut Vec<[f64; 3]>,
    threshold: f64,
    radius: f64,
    stats: &Stats,
) {
    let never = AtomicBool::new(false);
    let Some(mut chi2) = joint.chi2(theta) else {
        return;
    };
    let mut ranked: Vec<([f64; 3], f64)> = theta
        .iter()
        .filter_map(|t| {
            let rest: Vec<[f64; 3]> = theta.iter().filter(|u| *u != t).copied().collect();
            Some((*t, joint.chi2(&rest)? - chi2))
        })
        .collect();
    ranked.sort_by(|a, b| a.1.total_cmp(&b.1));
    let b = Budget {
        evaluations: 300,
        deadline: None,
        threads: cores(),
    };
    for (planet, _) in ranked.into_iter().take(2) {
        let rest: Vec<[f64; 3]> = theta.iter().filter(|u| **u != planet).copied().collect();
        let (free, fixed): (Vec<[f64; 3]>, Vec<[f64; 3]>) =
            rest.iter().partition(|t| distance(t, &planet) < radius);
        let local = Fit::new(
            shots,
            WorldParameters::default(),
            &never,
            false,
            fixed.iter().map(|t| planet_at(*t)).collect(),
        )
        .with_stats(stats);
        let refitted = if free.is_empty() {
            free
        } else {
            local.refine(&free, &b).map_or(free, |(t, _)| t)
        };
        let mut candidate = fixed;
        candidate.extend(refitted);
        if let Some(after) = joint.chi2(&candidate) {
            if after - chi2 < threshold {
                *theta = candidate;
                chi2 = after;
            }
        }
    }
}

/// One incremental update after path `n - 1` (0-based) arrived.
fn update(rp: &Replay, theta: &mut Vec<[f64; 3]>, n: usize, cfg: &Config) -> UpdateCost {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let ceilings = PartCeilings::default();
    let started = Instant::now();
    // Preparing the new path's windows at each level is part of the update.
    for level in &cfg.levels {
        std::hint::black_box(relative_shot(&rp.case.shots[n - 1].obs, *level, rp.noise));
    }
    let exact_nodes = nodes_of(&rp.case.shots[..n]);
    let (mut evals, mut equiv) = (0, 0.0);
    let mut phases = [0.0; 4];
    let mut lap = Instant::now();
    let mut tick = |slot: usize, phases: &mut [f64; 4]| {
        phases[slot] += lap.elapsed().as_secs_f64() * 1e3;
        lap = Instant::now();
    };
    for &level in &cfg.levels {
        let list = &rp.list(level)[..n];
        let shots: Cow<[Shot]> = if cfg.cap == 0 {
            Cow::Borrowed(list)
        } else {
            Cow::Owned(thin_shots(list, CELL, PER_CELL, cfg.cap, true, None))
        };
        let cost = nodes_of(&shots) as f64 / exact_nodes as f64;
        tick(0, &mut phases);
        let stats = Stats::default();
        let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats);
        // The warm refit needs no evidence; evidence (a Jacobian and a covariance) is only computed
        // when births or deaths are going to compare modes.
        let refit_theta = if theta.is_empty() {
            Vec::new()
        } else {
            let b = Budget {
                evaluations: cfg.refit,
                deadline: None,
                threads: cores(),
            };
            if cfg.local > 0.0 {
                // Planets near the new path are refit; the others are fixed field.
                let newest = rp.list(level)[n - 1].path.as_ref();
                let near = |t: &[f64; 3]| {
                    newest.is_some_and(|p| {
                        (0..p.row_count()).any(|r| {
                            let c = p.center(r);
                            (t[0] - c[0]).hypot(t[1] - c[1]) < cfg.local
                        })
                    })
                };
                let (free, fixed): (Vec<[f64; 3]>, Vec<[f64; 3]>) =
                    theta.iter().partition(|t| near(t));
                if free.is_empty() {
                    theta.clone()
                } else {
                    let local = Fit::new(
                        &shots,
                        world,
                        &never,
                        false,
                        fixed.iter().map(|t| planet_at(*t)).collect(),
                    )
                    .with_stats(&stats);
                    let mut out = fixed;
                    out.extend(local.refine(&free, &b).map_or(free, |(t, _)| t));
                    out
                }
            } else {
                joint
                    .refine(theta, &b)
                    .map_or_else(|| theta.clone(), |(t, _)| t)
            }
        };
        tick(1, &mut phases);
        let births_due = !seeds(&joint, &refit_theta, world).is_empty();
        let deaths_due = cfg.depth > 0 && cfg.local_death.is_none() && n % cfg.deaths_every == 0;
        if !(births_due || deaths_due) {
            tick(2, &mut phases);
            *theta = refit_theta;
            if let Some(threshold) = cfg.local_death.filter(|_| n % cfg.deaths_every == 0) {
                local_deaths(
                    &joint,
                    &shots,
                    theta,
                    threshold,
                    cfg.local.max(100.0),
                    &stats,
                );
            }
            tick(3, &mut phases);
            evals += stats.evaluations();
            equiv += stats.evaluations() as f64 * cost;
            continue;
        }
        let mut modes = vec![mode_at(&joint, &refit_theta).unwrap()];
        let l = limits(Some(cfg.births), &ceilings, 36000);
        let run = Run {
            limits: &l,
            stats: &stats,
        };
        let before = refit_theta.len();
        if births_due {
            grow(&joint, &shots, &mut modes, &run, world);
        }
        let born = best(&modes).planets.len() > before;
        tick(2, &mut phases);
        if cfg.depth > 0 && (deaths_due || born) {
            // The deaths budget counts from the evaluations already used by this level.
            let ld = limits(
                Some(stats.evaluations() + cfg.deaths.unwrap_or(cfg.births)),
                &ceilings,
                36000,
            );
            let drun = Run {
                limits: &ld,
                stats: &stats,
            };
            prune(&joint, &mut modes, &drun, cfg.depth);
        }
        *theta = theta_of_planets(&best(&modes).planets);
        if let Some(threshold) = cfg.local_death.filter(|_| n % cfg.deaths_every == 0) {
            local_deaths(
                &joint,
                &shots,
                theta,
                threshold,
                cfg.local.max(100.0),
                &stats,
            );
        }
        tick(3, &mut phases);
        evals += stats.evaluations();
        equiv += stats.evaluations() as f64 * cost;
    }
    UpdateCost {
        phases_ms: phases,
        wall_ms: started.elapsed().as_secs_f64() * 1e3,
        evals,
        equiv,
    }
}

/// Guarded exact refit (the E9c rule: accept when the exact chi2 falls and no planet moves 3 units
/// or chi2/row <= 1.5), at most 600 evaluations.
fn exact_polish(case: &Case, n: usize, theta: &[[f64; 3]]) -> Vec<[f64; 3]> {
    let never = AtomicBool::new(false);
    let exact = Fit::new(
        &case.shots[..n],
        WorldParameters::default(),
        &never,
        false,
        vec![],
    );
    let b = Budget {
        evaluations: 600,
        deadline: None,
        threads: cores(),
    };
    let Some((result, after)) = exact.refine(theta, &b) else {
        return theta.to_vec();
    };
    let before = exact.chi2(theta).unwrap();
    let rows = rows_of(&case.shots[..n]);
    if after < before && (displacement(theta, &result) < 3.0 || after / rows as f64 <= 1.5) {
        result
    } else {
        theta.to_vec()
    }
}

fn checkpoints(total: usize) -> Vec<usize> {
    let mut v: Vec<usize> = env_list("CHECKPOINTS", "4,8,16,32,60")
        .into_iter()
        .filter(|&n| n <= total)
        .collect();
    if !v.contains(&total) {
        v.push(total);
    }
    v
}

fn setup() -> (f64, Vec<Config>) {
    if let Some(t) = env_list("THREADS", "0").first() {
        TEST_THREADS.store(*t, std::sync::atomic::Ordering::Relaxed);
    }
    let noise: f64 = std::env::var("NOISE").map_or(0.0, |v| v.parse().unwrap());
    let configs = std::env::var("CONFIGS")
        .unwrap_or_else(|_| "lv=0.005".into())
        .split_whitespace()
        .map(Config::parse)
        .collect();
    (noise, configs)
}

#[test]
#[ignore]
fn diagnose_rebuild() {
    let (noise, _) = setup();
    let counts = env_list("COUNTS", "4,8,16,24,32,40,48,56,60");
    for case in cases(noise) {
        let mut ns: Vec<usize> = counts
            .iter()
            .copied()
            .filter(|&n| n <= case.shots.len())
            .collect();
        if !ns.contains(&case.shots.len()) {
            ns.push(case.shots.len());
        }
        for n in ns {
            let r = rebuild(&case, n, noise);
            let m = quality(&case, n, &r.theta);
            println!(
                "QUAL world={} cfg=rebuild n={n} {} evals={} equiv={:.0} wall={:.1}",
                world_id(&case),
                m.text,
                r.evals,
                r.equiv,
                r.wall
            );
        }
    }
}

#[test]
#[ignore]
fn diagnose_incremental() {
    let (noise, configs) = setup();
    let world = WorldParameters::default();
    let never = AtomicBool::new(false);
    let threads = cores();
    let planets = std::env::var("PLANETS").is_ok();
    let prod = std::env::var("PROD").is_ok();
    for case in cases(noise) {
        let id = world_id(&case);
        let total = case.shots.len();
        let cps = checkpoints(total);
        let truth = theta_of_planets(&case.truth);
        println!("WORLD {id} paths={total} threads={threads}");
        let later = rebuild(&case, total, noise).theta;
        let mut levels: Vec<f64> = vec![0.005, 0.02];
        for c in &configs {
            levels.extend(&c.levels);
        }
        levels.sort_by(|a, b| a.total_cmp(b));
        levels.dedup();
        let rp = Replay {
            case: &case,
            noise,
            full: levels
                .iter()
                .map(|&l| {
                    (
                        l,
                        case.shots
                            .iter()
                            .map(|s| relative_shot(&s.obs, l, noise))
                            .collect(),
                    )
                })
                .collect(),
        };
        for cfg in &configs {
            let mut theta: Vec<[f64; 3]> = Vec::new();
            for n in 1..=total {
                let cost = update(&rp, &mut theta, n, cfg);
                println!(
                    "UPD world={id} cfg={} n={n} k={} evals={} equiv={:.0} wall_ms={:.1} thin_ms={:.1} refit_ms={:.1} births_ms={:.1} deaths_ms={:.1} threads={threads}",
                    cfg.name,
                    theta.len(),
                    cost.evals,
                    cost.equiv,
                    cost.wall_ms,
                    cost.phases_ms[0],
                    cost.phases_ms[1],
                    cost.phases_ms[2],
                    cost.phases_ms[3]
                );
                if let Some((cadence, adopt)) = cfg.audit {
                    if n % cadence == 0 {
                        audit(&rp, &mut theta, n, cfg, adopt, noise);
                    }
                }
                if cps.contains(&n) {
                    let m = quality(&case, n, &theta);
                    println!("QUAL world={id} cfg={} n={n} {}", cfg.name, m.text);
                    // The map after a guarded exact refit on all rows (not kept in the state).
                    let polished = exact_polish(&case, n, &theta);
                    let m = quality(&case, n, &polished);
                    println!("QUAL world={id} cfg={}+x n={n} {}", cfg.name, m.text);
                    if planets {
                        let full05 = Fit::new(&rp.list(0.005)[..n], world, &never, false, vec![]);
                        let full02 = Fit::new(&rp.list(0.02)[..n], world, &never, false, vec![]);
                        let (b05, b02) =
                            (full05.chi2(&theta).unwrap(), full02.chi2(&theta).unwrap());
                        for i in 0..theta.len() {
                            let mut rest = theta.clone();
                            rest.remove(i);
                            // With the planets within 100 units of the removed one refit.
                            let (free, fixed): (Vec<[f64; 3]>, Vec<[f64; 3]>) =
                                rest.iter().partition(|t| distance(t, &theta[i]) < 100.0);
                            let local = Fit::new(
                                &rp.list(0.005)[..n],
                                world,
                                &never,
                                false,
                                fixed.iter().map(|t| planet_at(*t)).collect(),
                            );
                            let b = Budget {
                                evaluations: 300,
                                deadline: None,
                                threads: cores(),
                            };
                            let mut refitted = fixed;
                            refitted.extend(local.refine(&free, &b).map_or(free, |(t, _)| t));
                            let dlocal = full05.chi2(&refitted).unwrap() - b05;
                            println!(
                                "PLANET world={id} cfg={} n={n} i={i} d05={:.4e} d02={:.4e} dloc={dlocal:.4e} dtrue={:.2} dlater={:.2} r={:.1}",
                                cfg.name,
                                full05.chi2(&rest).unwrap() - b05,
                                full02.chi2(&rest).unwrap() - b02,
                                nearest_distance(&theta[i], &truth),
                                nearest_distance(&theta[i], &later),
                                theta[i][2]
                            );
                        }
                    }
                }
            }
        }
        if prod {
            run_production(&case, noise, &cps);
        }
    }
}

/// Audit: rebuild from scratch on the first `n` paths and compare it with the maintained map.
fn audit(rp: &Replay, theta: &mut Vec<[f64; 3]>, n: usize, cfg: &Config, adopt: bool, noise: f64) {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let case = rp.case;
    let rb = rebuild(case, n, noise);
    let full = Fit::new(&rp.list(0.005)[..n], world, &never, false, vec![]);
    let stats = Stats::default();
    let counted = Fit::new(&rp.list(0.005)[..n], world, &never, false, vec![]).with_stats(&stats);
    // The maintained map is refit on all rows at the audit level first: a fair comparison with the
    // rebuild, whose polish already ran there.
    let b = Budget {
        evaluations: 1500,
        deadline: None,
        threads: cores(),
    };
    let inc =
        refit(&counted, theta, &b).map_or_else(|| theta.clone(), |m| theta_of_planets(&m.planets));
    let evidence = |t: &[[f64; 3]]| full.evidence(t).map(|e| e.log_evidence);
    let (e_inc, e_rb) = (evidence(&inc), evidence(&rb.theta));
    let better = match (e_inc, e_rb) {
        (Some(a), Some(b)) => b > a,
        (None, Some(_)) => true,
        (Some(_), None) => false,
        (None, None) => full.chi2(&rb.theta).unwrap() < full.chi2(&inc).unwrap(),
    };
    let same = theta
        .iter()
        .filter(|p| nearest_distance(p, &rb.theta) <= 3.0)
        .count();
    let moved = theta
        .iter()
        .filter(|p| (3.0..=15.0).contains(&nearest_distance(p, &rb.theta)))
        .count();
    let removed = theta
        .iter()
        .filter(|p| nearest_distance(p, &rb.theta) > 15.0)
        .count();
    let added = rb
        .theta
        .iter()
        .filter(|p| nearest_distance(p, theta) > 15.0)
        .count();
    let (mi, mr) = (quality(case, n, theta), quality(case, n, &rb.theta));
    println!(
        "AUDIT world={} cfg={} n={n} k_inc={} k_rb={} same={same} moved={moved} removed={removed} added={added} ev_inc={} ev_rb={} better={better} adopt={adopt} rec2_inc={} rec2_rb={} spur_inc={} spur_rb={} ratio_inc={:.3e} ratio_rb={:.3e} fe_inc={:.3e} fe_rb={:.3e} rb_evals={} rb_equiv={:.0} rb_wall={:.1}",
        world_id(case),
        cfg.name,
        mi.k,
        mr.k,
        e_inc.map_or("none".into(), |v| format!("{v:.1}")),
        e_rb.map_or("none".into(), |v| format!("{v:.1}")),
        mi.rec2,
        mr.rec2,
        mi.spur,
        mr.spur,
        mi.ratio,
        mr.ratio,
        mi.explored,
        mr.explored,
        rb.evals,
        rb.equiv,
        rb.wall
    );
    if adopt && better {
        *theta = rb.theta;
    }
}

/// The production update path by path (exact shots at the production noise level, warm modes of
/// the previous update, default planet cap and time budget); cost and quality.
fn run_production(case: &Case, noise: f64, cps: &[usize]) {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let ceilings = PartCeilings::default();
    let threads = cores();
    let id = world_id(case);
    let shots: Vec<Shot> = case
        .shots
        .iter()
        .map(|s| Shot::new(s.obs.clone(), None, world, &[0.1_f64.max(noise)]))
        .collect();
    let mut warm: Vec<Vec<crate::protocol::Planet>> = Vec::new();
    for n in 1..=shots.len() {
        let stats = Stats::default();
        let l = limits(None, &ceilings, 5);
        let started = Instant::now();
        let Some((modes, limit)) = search(&shots[..n], world, &never, &warm, &l, &stats) else {
            continue;
        };
        let wall = started.elapsed().as_secs_f64() * 1e3;
        let mut sorted = modes.clone();
        sorted.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
        warm.clear();
        for m in &sorted {
            if warm.len() < 3 && !warm.iter().any(|w| same_map(w, &m.planets)) {
                warm.push(m.planets.clone());
            }
        }
        let theta = theta_of_planets(&best(&modes).planets);
        println!(
            "UPD world={id} cfg=prod n={n} k={} evals={} equiv={} wall_ms={wall:.1} threads={threads} limit={limit:?}",
            theta.len(),
            stats.evaluations(),
            stats.evaluations()
        );
        if cps.contains(&n) {
            println!(
                "QUAL world={id} cfg=prod n={n} {}",
                quality(case, n, &theta).text
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_keys_parse() {
        let c = Config::parse("lv=0.02/0.005:cap=1000:rf=300:ev=1500:lr=100:dl=30:dn=4:audit=16");
        assert_eq!(c.levels, vec![0.02, 0.005]);
        assert_eq!(
            (c.cap, c.refit, c.births, c.deaths_every),
            (1000, 300, 1500, 4)
        );
        assert_eq!(
            (c.local, c.local_death, c.audit),
            (100.0, Some(30.0), Some((16, true)))
        );
    }

    /// The bounded row set respects the row cap and the per-cell limit.
    #[test]
    fn thinning_bounds_rows_and_cells() {
        let case = synthetic(7001, 6, 3, 0.0);
        let relative: Vec<Shot> = case
            .shots
            .iter()
            .map(|s| relative_shot(&s.obs, 0.005, 0.0))
            .collect();
        let thin = thin_shots(&relative, CELL, PER_CELL, 150, true, None);
        let rows: usize = thin
            .iter()
            .filter_map(|s| s.path.as_ref())
            .map(|p| p.row_count())
            .sum();
        assert!(rows <= 150 && rows > 0);
        let mut cells = std::collections::HashMap::new();
        for p in thin.iter().filter_map(|s| s.path.as_ref()) {
            for r in 0..p.row_count() {
                let c = p.center(r);
                *cells
                    .entry(((c[0] / CELL).floor() as i64, (c[1] / CELL).floor() as i64))
                    .or_insert(0) += 1;
            }
        }
        assert!(cells.values().all(|&n| n <= PER_CELL));
    }
}
