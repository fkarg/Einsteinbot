//! P7: a resumable planet search that absorbs new paths while it runs (`docs/estimator-experiments.md`,
//! "P6 results" for the B* update it extends, "P7 results"). `Search` keeps the map, the thinned
//! row set (rebuilt from the paths so far at every step), the relative-noise level being searched,
//! and the pending birth work: candidate planets (alone fits) and partial joint refits. `add_path`
//! gives the new path the cheap local refit of B* and publishes the map; `step(budget)` does a
//! bounded slice of birth work and can be called any time. A path that arrives between slices only
//! re-scores the candidates and partial fits against the new rows (the next slice refits them
//! warm); it does not restart the search. Truth scores only.
//!
//! Run (release): `cargo test --release --lib diagnose_resumable -- --ignored --nocapture
//! --test-threads=1`. Environment as `diagnose_incremental` (`SEEDS`, `LIVE`, `PATHS`, `THREADS`,
//! `CHECKPOINTS`, `NOISE`) and `SPECS` (space separated; see `Spec::parse`). `SCHEDULE=<ms>`
//! switches from the deterministic slice replay to a wall-clock arrival schedule (a new path every
//! that many ms, slices in between), and then `B=<config spec of incremental.rs>` also runs the
//! blocking P6 update on the same schedule.
use super::incremental::{
    Config, Replay, audit, checkpoints, distance, env_list, exact_polish, nearest_distance,
    quality, relative_shot, setup, update, world_id,
};
use super::thinning::thin_shots;
use super::*;
use crate::estimator::fit::{Budget, planet_at};
use crate::estimator::search::mode_at;
use crate::estimator::work::{cores, par_map};

const CELL: f64 = 14.0;
const PER_CELL: usize = 2;
const MAX_PLANETS: usize = 32;
/// Joint refits kept per round, as in the production birth.
const JOINT: usize = 3;
/// Alone fits per batch (a batch is the unit between wall-clock checks).
const BATCH: usize = 8;
/// A joint partial that is still improving after this many slices of work counts as finished.
const MAX_ROUNDS: usize = 6;
/// Candidates closer than this (xy) are one hypothesis; the list is bounded.
const DUPLICATE: f64 = 30.0;
const MAX_CANDIDATES: usize = 24;
const REFIT: u64 = 300;
/// Weakest planets tried as deaths after a birth.
const DEATH_TRIALS: usize = 2;
const LOCAL_RADIUS: f64 = 100.0;

struct Cand {
    theta: [f64; 3],
    /// Chi2 of the map plus this planet when last scored; `None`: never fitted.
    chi2: Option<f64>,
}

struct Partial {
    /// The map plus the candidate planet (last), refit jointly.
    theta: Vec<[f64; 3]>,
    done: bool,
    rounds: usize,
}

pub(super) struct Search {
    levels: Vec<f64>,
    cap: usize,
    /// Throw the pending work away when a path arrives (the comparison variant).
    restart: bool,
    /// Death trial after every birth (the ablation switches it off).
    deaths: bool,
    noise: f64,
    /// Full-row relative shots of every path so far, per level.
    paths: Vec<Vec<Shot>>,
    /// The published map.
    pub theta: Vec<[f64; 3]>,
    /// Level the births are searched at; `levels.len()` when nothing is pending.
    level: usize,
    seeded: bool,
    cands: Vec<Cand>,
    /// Joint refits being settled: map plus a candidate (births) or map minus a weak planet
    /// (deaths, when `deaths_due`).
    partials: Vec<Partial>,
    /// A death trial (the two planets whose removal costs least chi2, refit jointly, kept when
    /// the evidence rises) follows every birth, as in B*.
    deaths_due: bool,
    /// Evaluations spent so far: alone fits, joint refits of births, of deaths (reporting only).
    pub work: [u64; 3],
}

impl Search {
    pub(super) fn new(
        levels: Vec<f64>,
        cap: usize,
        restart: bool,
        deaths: bool,
        noise: f64,
    ) -> Search {
        Search {
            paths: vec![Vec::new(); levels.len()],
            level: levels.len(),
            levels,
            cap,
            restart,
            deaths,
            noise,
            theta: Vec::new(),
            seeded: false,
            cands: Vec::new(),
            partials: Vec::new(),
            deaths_due: false,
            work: [0; 3],
        }
    }

    pub(super) fn idle(&self) -> bool {
        self.level >= self.levels.len()
    }

    fn thin(&self, level: usize) -> Vec<Shot> {
        thin_shots(&self.paths[level], CELL, PER_CELL, self.cap, true, None)
    }

    fn clear_pending(&mut self) {
        self.cands.clear();
        self.partials.clear();
        self.seeded = false;
        self.deaths_due = false;
    }

    /// Replace the map (an adopted rebuild); pending work no longer fits it.
    pub(super) fn set_map(&mut self, theta: Vec<[f64; 3]>) {
        self.theta = theta;
        self.clear_pending();
        self.level = 0;
    }

    /// A new path: the local refit of B* (planets within 100 units of its rows, per level, 300
    /// evaluations) and the pending work marked stale (or dropped when `restart`).
    pub(super) fn add_path(&mut self, obs: &ShotObservation) {
        let never = AtomicBool::new(false);
        let world = WorldParameters::default();
        for (level, rel) in self.levels.iter().enumerate() {
            self.paths[level].push(relative_shot(obs, *rel, self.noise));
        }
        for level in 0..self.levels.len() {
            if self.theta.is_empty() {
                break;
            }
            let shots = self.thin(level);
            let newest = self.paths[level].last().and_then(|s| s.path.as_ref());
            let near = |t: &[f64; 3]| {
                newest.is_some_and(|p| {
                    (0..p.row_count()).any(|r| {
                        let c = p.center(r);
                        (t[0] - c[0]).hypot(t[1] - c[1]) < LOCAL_RADIUS
                    })
                })
            };
            let (free, fixed): (Vec<[f64; 3]>, Vec<[f64; 3]>) =
                self.theta.iter().partition(|t| near(t));
            if free.is_empty() {
                continue;
            }
            let local = Fit::new(
                &shots,
                world,
                &never,
                false,
                fixed.iter().map(|t| planet_at(*t)).collect(),
            );
            let b = Budget {
                evaluations: REFIT,
                deadline: None,
                threads: cores(),
            };
            self.theta = fixed;
            self.theta
                .extend(local.refine(&free, &b).map_or(free, |(t, _)| t));
        }
        self.level = 0;
        self.seeded = false;
        if self.restart {
            self.clear_pending();
        } else {
            // Candidates keep their alone fits (re-scored on the new rows when the next round
            // ranks them); the joint partials resume from where they were.
            for p in &mut self.partials {
                p.done = false;
                p.rounds = 0;
            }
        }
    }

    /// One slice of birth work: about `evals` evaluations (checked between batches) and not past
    /// `deadline`. Returns the evaluations spent.
    pub(super) fn step(&mut self, evals: u64, deadline: Instant) -> u64 {
        let never = AtomicBool::new(false);
        let world = WorldParameters::default();
        let mut spent = 0;
        while !self.idle() && spent < evals && Instant::now() < deadline {
            let shots = self.thin(self.level);
            let joint = Fit::new(&shots, world, &never, false, vec![]).with_deadline(deadline);
            let left = evals - spent;
            if self.deaths_due && self.partials.is_empty() {
                let chi2 = joint.chi2(&self.theta);
                let mut weakest: Vec<(usize, f64)> = (0..self.theta.len())
                    .filter_map(|i| {
                        let mut rest = self.theta.clone();
                        rest.remove(i);
                        Some((i, joint.chi2(&rest)? - chi2?))
                    })
                    .collect();
                weakest.sort_by(|a, b| a.1.total_cmp(&b.1));
                self.partials = weakest
                    .iter()
                    .take(DEATH_TRIALS)
                    .map(|(i, _)| {
                        let mut theta = self.theta.clone();
                        theta.remove(*i);
                        Partial {
                            theta,
                            done: false,
                            rounds: 0,
                        }
                    })
                    .collect();
                if self.partials.is_empty() {
                    self.deaths_due = false;
                }
                continue;
            }
            if !self.seeded && self.partials.is_empty() {
                for s in seeds(&joint, &self.theta, world) {
                    // A seed near a kept candidate is the same hypothesis.
                    if !self
                        .cands
                        .iter()
                        .any(|c| distance(&c.theta, &s) < DUPLICATE)
                    {
                        self.cands.push(Cand {
                            theta: s,
                            chi2: None,
                        });
                    }
                }
                // Bounded list: fitted candidates by their last chi2, unfitted ones after.
                self.cands.sort_by(|a, b| {
                    a.chi2
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&b.chi2.unwrap_or(f64::INFINITY))
                });
                self.cands.truncate(MAX_CANDIDATES);
                self.seeded = true;
                if self.cands.is_empty() {
                    self.next_level();
                }
                continue;
            }
            // Candidates not fitted on the current rows and map: alone against the field of the map.
            let todo: Vec<usize> = (0..self.cands.len())
                .filter(|&i| self.partials.is_empty() && self.cands[i].chi2.is_none())
                .take(BATCH)
                .collect();
            if !todo.is_empty() {
                let stats = Stats::default();
                let alone = Fit::new(
                    &shots,
                    world,
                    &never,
                    false,
                    self.theta.iter().map(|t| planet_at(*t)).collect(),
                )
                .with_deadline(deadline)
                .with_stats(&stats);
                let b = Budget {
                    evaluations: (left / todo.len() as u64).clamp(15, 150),
                    deadline: Some(deadline),
                    threads: (cores() / todo.len()).max(1),
                };
                let fitted = par_map(&todo, cores(), |&i| {
                    alone.refine(&[self.cands[i].theta], &b)
                });
                for (&i, fit) in todo.iter().zip(fitted) {
                    self.cands[i] = match fit {
                        Some((t, chi2)) => Cand {
                            theta: t[0],
                            chi2: Some(chi2),
                        },
                        None => Cand {
                            theta: self.cands[i].theta,
                            chi2: Some(f64::INFINITY),
                        },
                    };
                }
                spent += stats.evaluations();
                self.work[0] += stats.evaluations();
                continue;
            }
            if self.partials.is_empty() {
                // Re-score every fitted candidate against the current rows and map.
                let rescored = {
                    let alone = Fit::new(
                        &shots,
                        world,
                        &never,
                        false,
                        self.theta.iter().map(|t| planet_at(*t)).collect(),
                    );
                    par_map(&self.cands, cores(), |c| alone.chi2(&[c.theta]))
                };
                for (c, chi2) in self.cands.iter_mut().zip(rescored) {
                    c.chi2 = Some(chi2.unwrap_or(f64::INFINITY));
                }
                let mut ranked: Vec<&Cand> = self
                    .cands
                    .iter()
                    .filter(|c| c.chi2.is_some_and(f64::is_finite))
                    .collect();
                ranked.sort_by(|a, b| a.chi2.unwrap().total_cmp(&b.chi2.unwrap()));
                let mut distinct: Vec<[f64; 3]> = Vec::new();
                for c in ranked {
                    if distinct.len() < JOINT
                        && !distinct.iter().any(|d| {
                            distance(d, &c.theta) < 10.0 && (d[2] - c.theta[2]).abs() < 3.0
                        })
                    {
                        distinct.push(c.theta);
                    }
                }
                self.partials = distinct
                    .into_iter()
                    .map(|c| {
                        let mut theta = self.theta.clone();
                        theta.push(c);
                        Partial {
                            theta,
                            done: false,
                            rounds: 0,
                        }
                    })
                    .collect();
                if self.partials.is_empty() {
                    self.next_level();
                    continue;
                }
            }
            // Joint refits, resumed from where the last slice left them.
            let todo: Vec<usize> = (0..self.partials.len())
                .filter(|&i| !self.partials[i].done)
                .collect();
            if !todo.is_empty() {
                let each = (left / todo.len() as u64).max(30);
                let b = Budget {
                    evaluations: each,
                    deadline: Some(deadline),
                    threads: (cores() / todo.len()).max(1),
                };
                let fits = par_map(&todo, cores(), |&i| {
                    let stats = Stats::default();
                    let fit = Fit::new(&shots, world, &never, false, vec![])
                        .with_deadline(deadline)
                        .with_stats(&stats);
                    let out = fit.refine(&self.partials[i].theta, &b);
                    (out, stats.evaluations())
                });
                for (&i, (out, used)) in todo.iter().zip(fits) {
                    let p = &mut self.partials[i];
                    p.rounds += 1;
                    if let Some((t, _)) = out {
                        p.theta = t;
                    }
                    p.done = used < each || p.rounds >= MAX_ROUNDS;
                    spent += used;
                    self.work[1 + usize::from(self.deaths_due)] += used;
                }
                continue;
            }
            // Every joint refit has settled: adopt the best if it beats the map's evidence.
            let parent = mode_at(&joint, &self.theta).map_or(f64::NEG_INFINITY, |m| m.log_evidence);
            let child = self
                .partials
                .iter()
                .filter_map(|p| mode_at(&joint, &p.theta).map(|m| (m.log_evidence, &p.theta)))
                .max_by(|a, b| a.0.total_cmp(&b.0));
            let deaths = self.deaths_due;
            self.deaths_due = false;
            match child {
                Some((evidence, theta))
                    if evidence > parent && (deaths || theta.len() <= MAX_PLANETS) =>
                {
                    let theta = theta.clone();
                    self.deaths_due = !deaths && self.deaths;
                    if !deaths {
                        // The candidates stay as warm starts (re-scored next round); the one just
                        // born is spent.
                        let born = *theta.last().unwrap();
                        self.cands.retain(|c| distance(&c.theta, &born) > 20.0);
                    }
                    self.theta = theta;
                    self.partials.clear();
                    self.seeded = false;
                }
                _ if deaths => self.partials.clear(),
                _ => self.next_level(),
            }
        }
        spent
    }

    fn next_level(&mut self) {
        self.level += 1;
        self.clear_pending();
    }
}

// ---------------------------------------------------------------------------------------------
// Harness

struct Spec {
    name: String,
    /// Evaluations per slice and slices after each path (slice replay).
    evals: u64,
    slices: usize,
    /// Wall-clock ceiling of a slice (ms).
    ceil_ms: u64,
    levels: Vec<f64>,
    cap: usize,
    restart: bool,
    deaths: bool,
    /// Run slices until idle after the last path (slice replay).
    settle: bool,
    /// Adopt (cached) from-scratch rebuilds every N paths when their evidence is higher (untimed,
    /// background work in a live system).
    audit: Option<usize>,
}

impl Spec {
    /// `sl=300:ns=2:ceil=300:lv=0.02/0.005:cap=1000:restart=0:dt=1:settle=1:audit=8`
    fn parse(text: &str) -> Spec {
        let mut s = Spec {
            name: text.to_string(),
            evals: 300,
            slices: 1,
            ceil_ms: 300,
            levels: vec![0.02, 0.005],
            cap: 1000,
            restart: false,
            deaths: true,
            settle: false,
            audit: None,
        };
        for kv in text.split(':') {
            let (k, v) = kv.split_once('=').unwrap();
            match k {
                "sl" => s.evals = v.parse().unwrap(),
                "ns" => s.slices = v.parse().unwrap(),
                "ceil" => s.ceil_ms = v.parse().unwrap(),
                "lv" => s.levels = v.split('/').map(|x| x.parse().unwrap()).collect(),
                "cap" => s.cap = v.parse().unwrap(),
                "restart" => s.restart = v == "1",
                "dt" => s.deaths = v == "1",
                "settle" => s.settle = v == "1",
                "audit" => s.audit = Some(v.parse().unwrap()),
                other => panic!("unknown key {other}"),
            }
        }
        s
    }
}

fn recall2(theta: &[[f64; 3]], truth: &[[f64; 3]]) -> (usize, usize) {
    let hit = truth
        .iter()
        .filter(|t| nearest_distance(t, theta) <= 2.0)
        .count();
    let spurious = theta
        .iter()
        .filter(|t| nearest_distance(t, truth) > 15.0)
        .count();
    (hit, spurious)
}

fn print_quality(case: &Case, id: &str, name: &str, n: usize, theta: &[[f64; 3]]) {
    println!(
        "QUAL world={id} cfg={name} n={n} {}",
        quality(case, n, theta).text
    );
    let polished = exact_polish(case, n, theta);
    println!(
        "QUAL world={id} cfg={name}+x n={n} {}",
        quality(case, n, &polished).text
    );
}

/// Deterministic replay: after every path the local refit, then `slices` slices of `evals`
/// evaluations. The latency of an update is the local refit plus one slice.
fn replay_slices(case: &Case, spec: &Spec, noise: f64, threads: usize) {
    let id = world_id(case);
    let total = case.shots.len();
    let cps = checkpoints(total);
    let mut search = Search::new(
        spec.levels.clone(),
        spec.cap,
        spec.restart,
        spec.deaths,
        noise,
    );
    println!(
        "WORLD {id} paths={total} threads={threads} cfg={}",
        spec.name
    );
    for n in 1..=total {
        let started = Instant::now();
        search.add_path(&case.shots[n - 1].obs);
        let add_ms = started.elapsed().as_secs_f64() * 1e3;
        let (mut evals, mut worst, mut slice_ms) = (0, 0.0_f64, 0.0);
        for _ in 0..spec.slices {
            let s = Instant::now();
            evals += search.step(spec.evals, s + Duration::from_millis(spec.ceil_ms));
            let ms = s.elapsed().as_secs_f64() * 1e3;
            worst = worst.max(ms);
            slice_ms += ms;
        }
        println!(
            "UPD world={id} cfg={} n={n} k={} evals={evals} equiv={evals} wall_ms={:.1} add_ms={add_ms:.1} slice_max_ms={worst:.1} slices_ms={slice_ms:.1} idle={} threads={threads}",
            spec.name,
            search.theta.len(),
            add_ms + worst,
            search.idle()
        );
        if spec.audit.is_some_and(|c| n % c == 0) {
            let mut theta = search.theta.clone();
            let list: Vec<Shot> = search.paths[search
                .levels
                .iter()
                .position(|l| *l == 0.005)
                .expect("audit needs level 0.005")]
            .clone();
            if audit(case, &list, &mut theta, n, &spec.name, true, noise) {
                search.set_map(theta);
            }
        }
        if cps.contains(&n) {
            print_quality(case, &id, &spec.name, n, &search.theta);
        }
    }
    println!(
        "WORK world={id} cfg={} alone={} births={} deaths={}",
        spec.name, search.work[0], search.work[1], search.work[2]
    );
    if spec.settle {
        let started = Instant::now();
        let mut evals = 0;
        let mut rounds = 0;
        while !search.idle() && rounds < 150 {
            evals += search.step(
                spec.evals,
                Instant::now() + Duration::from_millis(spec.ceil_ms),
            );
            rounds += 1;
        }
        println!(
            "SETTLE world={id} cfg={} n={total} slices={rounds} evals={evals} wall_ms={:.1} idle={}",
            spec.name,
            started.elapsed().as_secs_f64() * 1e3,
            search.idle()
        );
        print_quality(
            case,
            &id,
            &format!("{}+settle", spec.name),
            total,
            &search.theta,
        );
    }
}

/// Wall-clock arrival schedule: path `i` arrives `period` ms after path `i - 1`; between arrivals
/// the search takes slices. Prints the recall timeline (`TL` lines, truth scores only).
fn replay_schedule(case: &Case, spec: &Spec, noise: f64, period: u64, threads: usize) {
    let id = world_id(case);
    let truth = theta_of_planets(&case.truth);
    let total = case.shots.len();
    let mut search = Search::new(
        spec.levels.clone(),
        spec.cap,
        spec.restart,
        spec.deaths,
        noise,
    );
    let t0 = Instant::now();
    let period = Duration::from_millis(period);
    println!(
        "WORLD {id} paths={total} threads={threads} cfg={} period_ms={}",
        spec.name,
        period.as_millis()
    );
    let mut worst_late = 0.0_f64;
    let tl = |search: &Search, n: usize, what: &str| {
        let (hit, spur) = recall2(&search.theta, &truth);
        println!(
            "TL world={id} cfg={} period_ms={} n={n} t_ms={:.0} k={} rec2={hit} spur={spur} ev={what}",
            spec.name,
            period.as_millis(),
            t0.elapsed().as_secs_f64() * 1e3,
            search.theta.len()
        );
    };
    for n in 1..=total {
        let arrival = t0 + period * (n as u32 - 1);
        while Instant::now() < arrival {
            if search.idle() {
                std::thread::sleep(arrival.saturating_duration_since(Instant::now()));
                break;
            }
            let until = arrival.min(Instant::now() + Duration::from_millis(spec.ceil_ms));
            search.step(spec.evals, until);
            tl(&search, n - 1, "slice");
        }
        search.add_path(&case.shots[n - 1].obs);
        let late = Instant::now()
            .saturating_duration_since(arrival)
            .as_secs_f64()
            * 1e3;
        worst_late = worst_late.max(late);
        tl(&search, n, "path");
    }
    // The tail: one more period of slices after the last path.
    let end = t0 + period * total as u32;
    while Instant::now() < end && !search.idle() {
        let until = end.min(Instant::now() + Duration::from_millis(spec.ceil_ms));
        search.step(spec.evals, until);
        tl(&search, total, "slice");
    }
    println!(
        "TLEND world={id} cfg={} period_ms={} worst_publish_lag_ms={worst_late:.0}",
        spec.name,
        period.as_millis()
    );
    print_quality(case, &id, &spec.name, total, &search.theta);
}

/// The blocking P6 update on the same schedule: each path is handled when it arrives or when the
/// previous update has finished (a backlog is processed one path at a time).
fn replay_blocking(case: &Case, cfg: &Config, noise: f64, period: u64, threads: usize) {
    let id = world_id(case);
    let truth = theta_of_planets(&case.truth);
    let total = case.shots.len();
    let mut levels: Vec<f64> = cfg.levels.clone();
    levels.push(0.005);
    levels.sort_by(|a, b| a.total_cmp(b));
    levels.dedup();
    let rp = Replay {
        case,
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
    let t0 = Instant::now();
    let period = Duration::from_millis(period);
    println!(
        "WORLD {id} paths={total} threads={threads} cfg={} period_ms={}",
        cfg.name,
        period.as_millis()
    );
    let mut theta: Vec<[f64; 3]> = Vec::new();
    let mut worst_late = 0.0_f64;
    for n in 1..=total {
        let arrival = t0 + period * (n as u32 - 1);
        std::thread::sleep(arrival.saturating_duration_since(Instant::now()));
        update(&rp, &mut theta, n, cfg);
        let late = Instant::now()
            .saturating_duration_since(arrival)
            .as_secs_f64()
            * 1e3;
        worst_late = worst_late.max(late);
        let (hit, spur) = recall2(&theta, &truth);
        println!(
            "TL world={id} cfg={} period_ms={} n={n} t_ms={:.0} k={} rec2={hit} spur={spur} ev=path",
            cfg.name,
            period.as_millis(),
            t0.elapsed().as_secs_f64() * 1e3,
            theta.len()
        );
    }
    println!(
        "TLEND world={id} cfg={} period_ms={} worst_publish_lag_ms={worst_late:.0}",
        cfg.name,
        period.as_millis()
    );
    print_quality(case, &id, &cfg.name, total, &theta);
}

#[test]
#[ignore]
fn diagnose_resumable() {
    let (noise, _) = setup();
    let threads = cores();
    let specs: Vec<Spec> = std::env::var("SPECS")
        .unwrap_or_else(|_| "sl=300:ns=1".into())
        .split_whitespace()
        .map(Spec::parse)
        .collect();
    let schedule = env_list("SCHEDULE", "0")[0] as u64;
    let blocking: Vec<Config> = std::env::var("B")
        .map(|v| v.split_whitespace().map(Config::parse).collect())
        .unwrap_or_default();
    for case in cases(noise) {
        for spec in &specs {
            if schedule > 0 {
                replay_schedule(&case, spec, noise, schedule, threads);
            } else {
                replay_slices(&case, spec, noise, threads);
            }
        }
        if schedule > 0 {
            for cfg in &blocking {
                replay_blocking(&case, cfg, noise, schedule, threads);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that arrives mid-search is absorbed: pending candidates survive it, and `restart`
    /// drops them.
    #[test]
    fn new_path_keeps_pending_work() {
        let case = synthetic(7001, 6, 4, 0.0);
        let mut kept = Search::new(vec![0.02, 0.005], 400, false, true, 0.0);
        let mut restarted = Search::new(vec![0.02, 0.005], 400, true, true, 0.0);
        for s in [&mut kept, &mut restarted] {
            s.add_path(&case.shots[0].obs);
            s.add_path(&case.shots[1].obs);
            // Enough work for seeds and alone fits, not for a joint refit to settle.
            s.step(20, Instant::now() + Duration::from_secs(60));
        }
        assert!(!kept.cands.is_empty());
        kept.add_path(&case.shots[2].obs);
        restarted.add_path(&case.shots[2].obs);
        assert!(!kept.cands.is_empty());
        assert!(restarted.cands.is_empty() && restarted.partials.is_empty());
        assert_eq!(kept.level, 0);
    }

    /// Slices make progress and the search ends idle with a map that explains the paths better
    /// than an empty one.
    #[test]
    fn slices_grow_the_map_until_idle() {
        let case = synthetic(7001, 4, 6, 0.0);
        let mut s = Search::new(vec![0.02, 0.005], 600, false, true, 0.0);
        for shot in &case.shots {
            s.add_path(&shot.obs);
        }
        let mut rounds = 0;
        while !s.idle() && rounds < 200 {
            s.step(300, Instant::now() + Duration::from_secs(60));
            rounds += 1;
        }
        assert!(s.idle(), "not idle after {rounds} slices");
        assert!(!s.theta.is_empty());
    }
}
