//! Fog calibration against the disclosed truth of recorded maps (ignored test `fog_calibration`;
//! truth only scores). For selected decisions of each map epoch (the first whose belief had
//! fitted at least 8, 16, 32, 64, 128, 256 paths, and the last), the recorded belief's fog is
//! rebuilt and compared with the true planets the belief misses (none within 10 units):
//! - `CNT`: expected unseen count and its 90% interval against the missing count;
//! - `REL`: risk reliability on a 15-unit grid off every planet (predicted vs fraction of cells
//!   with a missing planet's centre within 100 units), per variant of the unseen count;
//! - `PULL`: the share of cells whose missing planets' field (and the total field error of the
//!   belief) is within 1 and 2 times the pull, overall and within 100 units of a missing planet.
//!
//! Environment: `DB` (default `einsteinbot.sqlite3`), `CAPTURES` (comma list).
//! Run: `DB=... CAPTURES=58,60 cargo test --release --lib fog_calibration -- --ignored --nocapture`.
use super::*;
use crate::protocol::{RecordedEvent, ServerEvent};
use rusqlite::Connection;
use serde_json::Value;

const GRID: f64 = 15.0;
const CHECKPOINTS: [usize; 6] = [8, 16, 32, 64, 128, 256];

struct Epoch {
    truth: Vec<Planet>,
    /// (sequence, observation)
    paths: Vec<(i64, ShotObservation)>,
    /// (sequence, belief json)
    decisions: Vec<(i64, Value)>,
}

fn epochs(db: &str, capture: i64) -> Vec<Epoch> {
    let connection = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open db");
    let mut statement = connection
        .prepare("SELECT sequence, event_json FROM events WHERE capture_id=?1 AND direction='inbound' AND event_type IN ('planets','shot_fin') ORDER BY sequence")
        .unwrap();
    let events: Vec<(i64, String)> = statement
        .query_map([capture], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let mut statement = connection
        .prepare("SELECT sequence, data_json FROM diagnostics WHERE capture_id=?1 AND kind='target_decision' ORDER BY sequence")
        .unwrap();
    let decisions: Vec<(i64, Value)> = statement
        .query_map([capture], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .filter_map(|(s, j)| {
            let v: Value = serde_json::from_str(&j).ok()?;
            (!v["map_belief"].is_null()).then(|| (s, v["map_belief"].clone()))
        })
        .collect();
    let mut out: Vec<(i64, Epoch)> = Vec::new();
    for (sequence, json) in events {
        let Ok(RecordedEvent::Inbound(event)) = serde_json::from_str::<RecordedEvent>(&json) else {
            continue;
        };
        match &event {
            ServerEvent::Planets { planets } => {
                if out.last().is_none_or(|(_, e)| e.truth != *planets) {
                    out.push((
                        sequence,
                        Epoch {
                            truth: planets.clone(),
                            paths: Vec::new(),
                            decisions: Vec::new(),
                        },
                    ));
                }
            }
            ServerEvent::ShotFin { .. } => {
                if let (Some((_, epoch)), Ok(obs)) =
                    (out.last_mut(), ShotObservation::try_from(&event))
                {
                    if !epoch.paths.iter().any(|(_, o)| *o == obs) {
                        epoch.paths.push((sequence, obs));
                    }
                }
            }
            _ => {}
        }
    }
    let starts: Vec<i64> = out.iter().map(|(s, _)| *s).collect();
    for (sequence, belief) in decisions {
        if let Some(i) = starts.iter().rposition(|s| *s < sequence) {
            out[i].1.decisions.push((sequence, belief));
        }
    }
    out.into_iter().map(|(_, e)| e).collect()
}

/// Truth planets with no belief planet within 10 units (greedy nearest pairs).
fn missing(truth: &[Planet], belief: &[Planet]) -> Vec<Planet> {
    let mut pairs = Vec::new();
    for (i, t) in truth.iter().enumerate() {
        for (j, b) in belief.iter().enumerate() {
            let d = (t.x - b.x).hypot(t.y - b.y);
            if d <= 10.0 {
                pairs.push((d, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let (mut tm, mut bm) = (vec![false; truth.len()], vec![false; belief.len()]);
    for (_, i, j) in pairs {
        if !tm[i] && !bm[j] {
            tm[i] = true;
            bm[j] = true;
        }
    }
    truth
        .iter()
        .zip(tm)
        .filter(|(_, m)| !m)
        .map(|(p, _)| p.clone())
        .collect()
}

/// The prior given `N >= found`, without the binomial (the P18 noise's unseen count).
fn prior_only(prior: CountPrior, found: usize) -> CountPosterior {
    let weights: Vec<f64> = (found..=COUNT_MAX)
        .map(|n| {
            if prior.sd <= 0.0 {
                (n as f64 == prior.centre.round()) as u8 as f64
            } else {
                (-0.5 * ((n as f64 - prior.centre) / prior.sd).powi(2)).exp()
            }
        })
        .collect();
    let sum: f64 = weights.iter().sum();
    let mut probabilities: Vec<f64> = weights.iter().map(|w| w / sum.max(1e-300)).collect();
    if sum == 0.0 {
        probabilities[0] = 1.0;
    }
    CountPosterior {
        found,
        probabilities,
    }
}

fn model_of(belief: &Value) -> (FogModel, &'static str) {
    if let Ok(model) = serde_json::from_value::<FogModel>(belief["fog_model"].clone()) {
        return (model, "recorded");
    }
    match belief["residual_model"]["total_planets"].as_u64() {
        Some(n) => (FogModel::known(n as usize), "known"),
        None => (FogModel::DEFAULT, "default"),
    }
}

#[derive(Default)]
struct Reliability {
    bins: [(f64, f64, f64); 10],
    brier: f64,
    hits: f64,
    n: f64,
}

impl Reliability {
    fn add(&mut self, p: f64, hit: bool) {
        let b = ((p * 10.0) as usize).min(9);
        let h = hit as u8 as f64;
        self.bins[b].0 += 1.0;
        self.bins[b].1 += p;
        self.bins[b].2 += h;
        self.brier += (p - h).powi(2);
        self.hits += h;
        self.n += 1.0;
    }

    fn line(&self) -> String {
        let base = self.hits / self.n.max(1.0);
        let bins: Vec<String> = self
            .bins
            .iter()
            .filter(|b| b.0 > 0.0)
            .map(|b| format!("{:.3}/{:.3}/{}", b.1 / b.0, b.2 / b.0, b.0))
            .collect();
        format!(
            "n={} brier={:.4} climatology={:.4} bins={}",
            self.n,
            self.brier / self.n.max(1.0),
            base * (1.0 - base),
            bins.join(" ")
        )
    }
}

#[derive(Default)]
struct Coverage2 {
    n: f64,
    in1: f64,
    in2: f64,
}

impl Coverage2 {
    fn add(&mut self, error: f64, sd: f64) {
        self.n += 1.0;
        self.in1 += (error <= sd) as u8 as f64;
        self.in2 += (error <= 2.0 * sd) as u8 as f64;
    }
    fn line(&self) -> String {
        format!(
            "n={} in1={:.3} in2={:.3}",
            self.n,
            self.in1 / self.n.max(1.0),
            self.in2 / self.n.max(1.0)
        )
    }
}

#[test]
#[ignore]
fn fog_calibration() {
    let db = std::env::var("DB").unwrap_or_else(|_| "einsteinbot.sqlite3".into());
    let captures: Vec<i64> = std::env::var("CAPTURES")
        .unwrap_or_else(|_| "58,60,67,69".into())
        .split(',')
        .filter_map(|c| c.parse().ok())
        .collect();
    let world = WorldParameters::default();
    let names = [
        "posterior",
        "prior",
        "r100",
        "r150",
        "r200",
        "fmiss^1",
        "fmiss^2",
    ];
    // Per variant: log score of the missing count, bias, 90% interval coverage, decisions.
    let mut scores = vec![(0.0, 0.0, 0.0, 0.0); names.len()];
    let mut reliability: Vec<Reliability> = names.iter().map(|_| Reliability::default()).collect();
    let mut pull = [
        Coverage2::default(),
        Coverage2::default(),
        Coverage2::default(),
        Coverage2::default(),
    ];
    for capture in captures {
        for (e, epoch) in epochs(&db, capture).into_iter().enumerate() {
            let mut selected = Vec::new();
            let mut done = Vec::new();
            for (sequence, belief) in &epoch.decisions {
                let fitted = belief["paths_fitted"].as_u64().map_or_else(
                    || epoch.paths.iter().filter(|(s, _)| s < sequence).count(),
                    |n| n as usize,
                );
                for c in CHECKPOINTS {
                    if fitted >= c && !done.contains(&c) {
                        done.push(c);
                        selected.push((fitted, belief));
                    }
                }
            }
            if let Some((sequence, belief)) = epoch.decisions.last() {
                let fitted = epoch.paths.iter().filter(|(s, _)| s < sequence).count();
                selected.push((fitted, belief));
            }
            selected.dedup_by_key(|s| s.0);
            for (fitted, belief) in selected {
                let planets: Vec<Planet> =
                    serde_json::from_value(belief["planets"].clone()).unwrap_or_default();
                let observations: Vec<ShotObservation> = epoch
                    .paths
                    .iter()
                    .take(fitted)
                    .map(|(_, o)| o.clone())
                    .collect();
                let (model, source) = model_of(belief);
                let mut coverage = Coverage::new(world);
                coverage.cover_observations(&observations);
                let started = std::time::Instant::now();
                let fog = Fog::build(&planets, &mut coverage, model);
                let build_ms = started.elapsed().as_secs_f64() * 1e3;
                // The live estimator extends its coverage by the newest path only.
                let mut extend = Coverage::new(world);
                if fitted > 1 {
                    extend.cover_observations(&observations[..fitted - 1]);
                    extend.centre_distances();
                }
                let started = std::time::Instant::now();
                extend.cover_observations(&observations);
                let live = Fog::build(&planets, &mut extend, model);
                let live_ms = started.elapsed().as_secs_f64() * 1e3;
                assert_eq!(live.summary(), fog.summary());
                println!("COST paths={fitted} build_ms={build_ms:.3} live_ms={live_ms:.3}");
                let missed = missing(&epoch.truth, &planets);
                // Variants of the unseen count: the prior's mean excess alone (P18), and the
                // binomial with P25's hidden fraction at 200 units.
                // 90% interval of the posterior's unseen count.
                let interval = |p: &CountPosterior| {
                    let (mut acc, mut lo, mut hi) = (0.0, None, 0);
                    for (i, q) in p.probabilities.iter().enumerate() {
                        acc += q;
                        if lo.is_none() && acc >= 0.05 {
                            lo = Some(i);
                        }
                        if acc <= 0.95 + 1e-12 {
                            hi = i + 1;
                        }
                    }
                    (lo.unwrap_or(0), hi.min(p.probabilities.len() - 1))
                };
                let mut variants = vec![fog.clone()];
                let mut prior = fog.clone();
                prior.posterior = prior_only(model.prior, planets.len());
                variants.push(prior);
                for reach in [100.0, 150.0, 200.0] {
                    let f = coverage.hidden_fraction(&planets, reach);
                    let mut v = fog.clone();
                    v.posterior = CountPosterior::new(
                        model.prior,
                        planets.len(),
                        f,
                        model.lower,
                        model.upper,
                    );
                    variants.push(v);
                }
                for power in [1.0, 2.0] {
                    let mut v = fog.clone();
                    v.posterior = CountPosterior::new(
                        model.prior,
                        planets.len(),
                        fog.miss_fraction.powf(power),
                        model.lower,
                        model.upper,
                    );
                    variants.push(v);
                }
                let f200 = coverage.hidden_fraction(&planets, 200.0);
                for (i, v) in variants.iter().enumerate() {
                    let p = v
                        .posterior
                        .probabilities
                        .get(missed.len())
                        .copied()
                        .unwrap_or(0.0);
                    scores[i].0 += p.max(1e-12).ln();
                    scores[i].1 += v.expected_unseen() - missed.len() as f64;
                    let (lo, hi) = interval(&v.posterior);
                    scores[i].2 += (lo <= missed.len() && missed.len() <= hi) as u8 as f64;
                    scores[i].3 += 1.0;
                }
                let counts: Vec<String> = variants
                    .iter()
                    .zip(names.iter())
                    .map(|(f, name)| {
                        let (lo, hi) = interval(&f.posterior);
                        format!("{name}={:.2}[{lo},{hi}]", f.expected_unseen())
                    })
                    .collect();
                println!(
                    "CNT capture={capture} epoch={e} paths={fitted} K={} truth={} missing={} f={:.4} f200={:.4} model={source} {}",
                    planets.len(),
                    epoch.truth.len(),
                    missed.len(),
                    fog.miss_fraction,
                    f200,
                    counts.join(" ")
                );
                let belief_map = crate::map::PlanetMap {
                    planets: planets.clone(),
                };
                let truth_map = crate::map::PlanetMap {
                    planets: epoch.truth.clone(),
                };
                let missing_map = crate::map::PlanetMap {
                    planets: missed.clone(),
                };
                let mut x = GRID / 2.0;
                while x < world.battlefield_width {
                    let mut y = GRID / 2.0;
                    while y < world.battlefield_height {
                        let p = Vec2::new(x, y);
                        y += GRID;
                        if epoch
                            .truth
                            .iter()
                            .chain(&planets)
                            .any(|q| (q.x - p.x).hypot(q.y - p.y) <= q.radius + 2.0)
                        {
                            continue;
                        }
                        let near = missed
                            .iter()
                            .any(|q| (q.x - p.x).hypot(q.y - p.y) <= RISK_RADIUS);
                        for (r, f) in reliability.iter_mut().zip(&variants) {
                            r.add(f.risk_at(p), near);
                        }
                        let sd = fog.pull_at(p);
                        let (Ok(t), Ok(b), Ok(m)) = (
                            truth_map.acceleration(p),
                            belief_map.acceleration(p),
                            missing_map.acceleration(p),
                        ) else {
                            continue;
                        };
                        let total = (t.x - b.x).hypot(t.y - b.y);
                        let unseen = m.x.hypot(m.y);
                        pull[0].add(unseen, sd);
                        pull[1].add(total, sd);
                        if near {
                            pull[2].add(unseen, sd);
                            pull[3].add(total, sd);
                        }
                    }
                    x += GRID;
                }
            }
        }
    }
    for (s, name) in scores.iter().zip(names) {
        println!(
            "SCORE {name} log={:.2} bias={:.2} cover90={:.3} n={}",
            s.0 / s.3,
            s.1 / s.3,
            s.2 / s.3,
            s.3
        );
    }
    for (r, name) in reliability.iter().zip(names) {
        println!("REL {name} {}", r.line());
    }
    for (c, name) in pull.iter().zip([
        "unseen_all",
        "total_all",
        "unseen_near_missing",
        "total_near_missing",
    ]) {
        println!("PULL {name} {}", c.line());
    }
}
