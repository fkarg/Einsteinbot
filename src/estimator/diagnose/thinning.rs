//! E9: row thinning and a relative-error noise term for the SEARCH stage only (`docs/
//! estimator-experiments.md`). The search stage grows K greedily on an approximate objective; the
//! result is then refit on the exact objective, the exact evidence picks the mode, and (staged
//! variants) exact births continue with the remaining budget and exact deaths prune. Truth scores
//! only. Run with
//! `cargo test --release --lib diagnose_thinning -- --ignored --nocapture --test-threads=1`.
//! Environment: `VARIANTS` (comma list: `exact`, `thin:CELL:PER_CELL:CAP:strong|spread`,
//! `rel:0.02`, `thinrel:CELL:PER_CELL:CAP:strong:0.02`), `BUDGET` (total evaluations, default
//! 20000), `STAGE1` (share of it for the search stage, default 0.5), `NOISE`, plus the
//! `SEEDS`/`LIVE`/`KS`/`PATHS` of `cases`.
use super::*;
use crate::estimator::search::{Mode, deaths, refit};
use crate::estimator::work::cores;

#[derive(Clone, Copy, Debug)]
struct Variant {
    /// Thinning: cell size, rows per cell, row cap, prefer strongly accelerated rows.
    thin: Option<(f64, usize, usize, bool)>,
    relative: f64,
}

fn parse(spec: &str) -> Variant {
    let p: Vec<&str> = spec.split(':').collect();
    let thin = |p: &[&str]| {
        (
            p[0].parse().unwrap(),
            p[1].parse().unwrap(),
            p[2].parse().unwrap(),
            p[3] == "strong",
        )
    };
    match p[0] {
        "exact" => Variant {
            thin: None,
            relative: 0.0,
        },
        "rel" => Variant {
            thin: None,
            relative: p[1].parse().unwrap(),
        },
        "thin" => Variant {
            thin: Some(thin(&p[1..])),
            relative: 0.0,
        },
        "thinrel" => Variant {
            thin: Some(thin(&p[1..5])),
            relative: p[5].parse().unwrap(),
        },
        other => panic!("unknown variant {other}"),
    }
}

/// The shots of the search stage: relative noise term and/or thinned rows.
fn search_shots(case: &Case, variant: Variant, noise: f64) -> Vec<Shot> {
    let world = WorldParameters::default();
    let mut shots: Vec<Shot> = case
        .shots
        .iter()
        .map(|shot| Shot {
            obs: shot.obs.clone(),
            contact: None,
            path: PathData::new_relative(
                &shot.obs.points,
                shot.obs.angle_degrees,
                shot.obs.velocity,
                world,
                &[noise],
                variant.relative,
            ),
        })
        .collect();
    if let Some((cell, per_cell, cap, strong)) = variant.thin {
        // (shot, row, strength, cell)
        let mut candidates: Vec<(usize, usize, f64, (i64, i64))> = Vec::new();
        for (s, shot) in shots.iter().enumerate() {
            let Some(path) = &shot.path else { continue };
            for r in 0..path.row_count() {
                let c = path.center(r);
                candidates.push((
                    s,
                    r,
                    path.strength(r),
                    ((c[0] / cell).floor() as i64, (c[1] / cell).floor() as i64),
                ));
            }
        }
        if strong {
            candidates.sort_by(|a, b| b.2.total_cmp(&a.2));
        } else {
            // Deterministic pseudo-random order, independent of strength.
            let hash = |s: usize, r: usize| {
                let mut state = ((s as u64) << 32) | r as u64;
                uniform(&mut state);
                (uniform(&mut state) * 1e9) as u64
            };
            candidates.sort_by_key(|c| hash(c.0, c.1));
        }
        let mut count: std::collections::HashMap<(i64, i64), usize> = Default::default();
        let mut kept: Vec<Vec<usize>> = vec![Vec::new(); shots.len()];
        let mut total = 0;
        for (s, r, _, cell) in candidates {
            if total >= cap {
                break;
            }
            let n = count.entry(cell).or_default();
            if *n < per_cell {
                *n += 1;
                kept[s].push(r);
                total += 1;
            }
        }
        for (shot, mut keep) in shots.iter_mut().zip(kept) {
            keep.sort_unstable();
            shot.path = shot.path.as_ref().and_then(|p| p.thinned(&keep));
        }
    }
    shots
}

fn work_done(run: &Run) -> bool {
    run.limits
        .max_evaluations
        .is_some_and(|m| run.stats.evaluations() >= m)
}

fn grow(joint: &Fit, shots: &[Shot], modes: &mut Vec<Mode>, run: &Run, world: WorldParameters) {
    let never = AtomicBool::new(false);
    loop {
        let parent = best(modes).clone();
        if parent.planets.len() >= run.limits.max_planets || work_done(run) {
            break;
        }
        let children = births(shots, joint, &parent, world, &never, run);
        let improved = children
            .first()
            .is_some_and(|c| c.log_evidence > parent.log_evidence);
        modes.extend(children);
        if !improved {
            break;
        }
    }
}

fn prune(joint: &Fit, modes: &mut Vec<Mode>, run: &Run) {
    let mut parent = best(modes).clone();
    for _ in 0..6 {
        if parent.planets.is_empty() || work_done(run) {
            break;
        }
        let children = deaths(joint, &parent, run);
        let Some(next) = children.first().cloned() else {
            break;
        };
        modes.extend(children);
        parent = next;
    }
}

struct Score {
    k: usize,
    recall2: usize,
    recall15: usize,
    spurious: usize,
    ratio: f64,
}

fn score(theta: &[[f64; 3]], truth: &[[f64; 3]], exact: &Fit, rows: usize) -> Score {
    let near = |t: &[f64; 3], limit: f64| {
        truth
            .iter()
            .any(|p| (t[0] - p[0]).hypot(t[1] - p[1]) < limit)
    };
    let hit = |p: &[f64; 3], limit: f64| {
        theta
            .iter()
            .any(|t| (t[0] - p[0]).hypot(t[1] - p[1]) < limit)
    };
    Score {
        k: theta.len(),
        recall2: truth.iter().filter(|p| hit(p, 2.0)).count(),
        recall15: truth.iter().filter(|p| hit(p, 15.0)).count(),
        spurious: theta.iter().filter(|t| !near(t, 15.0)).count(),
        ratio: chi2_per_row(exact, theta, rows) / chi2_per_row(exact, truth, rows),
    }
}

#[test]
#[ignore]
fn diagnose_thinning_search() {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let noise: f64 = std::env::var("NOISE").map_or(0.0, |v| v.parse().unwrap());
    let budget: u64 = std::env::var("BUDGET").map_or(20_000, |v| v.parse().unwrap());
    let stage1: f64 = std::env::var("STAGE1").map_or(0.5, |v| v.parse().unwrap());
    let variants: Vec<String> = std::env::var("VARIANTS")
        .unwrap_or_else(|_| "exact".into())
        .split(',')
        .map(String::from)
        .collect();
    // Score the growth stage alone: no exact refit, no deaths (for exact: no deaths).
    let growth_only = std::env::var("GROWTH_ONLY").is_ok();
    let anneal: Vec<f64> = std::env::var("ANNEAL")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| v.parse().ok())
        .collect();
    let ceilings = PartCeilings::default();
    for case in cases(noise) {
        let rows = rows(&case);
        let truth = theta_of_planets(&case.truth);
        let exact_fit = Fit::new(&case.shots, world, &never, false, vec![]);
        let supported_count = supported(&exact_fit, &truth).len();
        println!(
            "== {} rows {rows} nodes {} K_true {} supported {supported_count} truth chi2/row {:.2}",
            case.name,
            nodes(&case),
            truth.len(),
            chi2_per_row(&exact_fit, &truth, rows)
        );
        for spec in &variants {
            let variant = parse(spec);
            let staged = variant.thin.is_some() || variant.relative > 0.0;
            let stats = Stats::default();
            let limits = |max: u64| Limits {
                max_planets: 32,
                deadline: Instant::now() + Duration::from_secs(36000),
                max_evaluations: Some(max),
                parallel_evaluations: true,
                ceilings: &ceilings,
            };
            let started = Instant::now();
            let search = search_shots(&case, variant, noise);
            let search_rows: usize = search
                .iter()
                .filter_map(|s| s.path.as_ref())
                .map(|p| 2 * p.row_count())
                .sum();
            let search_nodes: usize = search
                .iter()
                .filter_map(|s| s.path.as_ref())
                .map(|p| p.nodes.len())
                .sum();
            let cost_ratio = search_nodes as f64 / nodes(&case) as f64;
            let exact = Fit::new(&case.shots, world, &never, false, vec![]).with_stats(&stats);
            let mut stage1_score = None;
            let (mut evals1, mut wall1) = (0, 0.0);
            let modes = if !staged {
                let l = limits(budget);
                let run = Run {
                    limits: &l,
                    stats: &stats,
                };
                let mut modes = vec![mode_at(&exact, &[]).unwrap()];
                grow(&exact, &case.shots, &mut modes, &run, world);
                if !growth_only {
                    prune(&exact, &mut modes, &run);
                }
                modes
            } else {
                let l1 = limits((budget as f64 * stage1) as u64);
                let run1 = Run {
                    limits: &l1,
                    stats: &stats,
                };
                let sjoint = Fit::new(&search, world, &never, false, vec![]).with_stats(&stats);
                let mut smodes = vec![mode_at(&sjoint, &[]).unwrap()];
                grow(&sjoint, &search, &mut smodes, &run1, world);
                let found = theta_of_planets(&best(&smodes).planets);
                stage1_score = Some(score(&found, &truth, &exact_fit, rows));
                evals1 = stats.evaluations();
                wall1 = started.elapsed().as_secs_f64();
                if growth_only {
                    // Score the search stage's own best map; no exact refit.
                    smodes
                } else {
                    // Stage 2: exact refit, exact births with the rest, exact deaths.
                    let l2 = limits(budget);
                    let run2 = Run {
                        limits: &l2,
                        stats: &stats,
                    };
                    let mut modes = vec![mode_at(&exact, &[]).unwrap()];
                    let b = Budget {
                        evaluations: budget.saturating_sub(stats.evaluations()).max(500),
                        deadline: None,
                        threads: cores(),
                    };
                    // Optional continuation: refit the whole map on full rows with decreasing
                    // relative noise before the exact refit (`ANNEAL=0.1,0.03,0.01`).
                    let mut found = found;
                    for level in anneal.iter() {
                        let shots = search_shots(
                            &case,
                            Variant {
                                thin: None,
                                relative: *level,
                            },
                            noise,
                        );
                        let fit = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats);
                        if let Some((theta, _)) = fit.refine(&found, &b) {
                            found = theta;
                        }
                        println!(
                            "   anneal {level}: ratio {:.3e} rec2 {}",
                            score(&found, &truth, &exact_fit, rows).ratio,
                            score(&found, &truth, &exact_fit, rows).recall2
                        );
                    }
                    modes.extend(refit(&exact, &found, &b));
                    grow(&exact, &case.shots, &mut modes, &run2, world);
                    prune(&exact, &mut modes, &run2);
                    modes
                }
            };
            let wall = started.elapsed().as_secs_f64();
            let theta = theta_of_planets(&best(&modes).planets);
            let s = score(&theta, &truth, &exact_fit, rows);
            let s1 = stage1_score.map_or(String::new(), |s| {
                format!(
                    " | stage1 K {} rec2 {} rec15 {} spur {} ratio {:.3e}",
                    s.k, s.recall2, s.recall15, s.spurious, s.ratio
                )
            });
            println!(
                "RESULT {} | {spec} | rows {search_rows} nodes_ratio {cost_ratio:.2} | K {} rec2 {}/{} rec15 {} spur {} ratio {:.3e} | evals {} (stage1 {evals1}, equiv {:.0}) wall {wall:.1}s (stage1 {wall1:.1}s){s1}",
                case.name,
                s.k,
                s.recall2,
                truth.len(),
                s.recall15,
                s.spurious,
                s.ratio,
                stats.evaluations(),
                evals1 as f64 * cost_ratio + (stats.evaluations() - evals1) as f64,
            );
        }
    }
}
