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
use crate::estimator::fit::Budget;
use crate::estimator::search::seeds;
use crate::estimator::search::{Mode, deaths, refit};
use crate::estimator::work::{cores, par_map};

#[derive(Clone, Copy, Debug)]
pub(super) struct Variant {
    /// Thinning: cell size, rows per cell, row cap, prefer strongly accelerated rows.
    pub thin: Option<(f64, usize, usize, bool)>,
    pub relative: f64,
    /// Re-thin after every growth round by the residual of the current map (strength = whitened
    /// residual magnitude under the exact noise) instead of the observed acceleration.
    pub resid: bool,
    /// After the search-stage growth stalls, keep adding planets with the found ones frozen as
    /// fixed field, re-thinning by the raw residual of the frozen map each round.
    pub frozen: bool,
}

pub(super) fn parse(spec: &str) -> Variant {
    let p: Vec<&str> = spec.split(':').collect();
    let thin = |p: &[&str]| {
        (
            p[0].parse().unwrap(),
            p[1].parse().unwrap(),
            p[2].parse().unwrap(),
            p[3] == "strong",
        )
    };
    let resid = p.contains(&"resid");
    let frozen = p.contains(&"frozen");
    match p[0] {
        "exact" => Variant {
            thin: None,
            relative: 0.0,
            resid,
            frozen,
        },
        "rel" => Variant {
            thin: None,
            relative: p[1].parse().unwrap(),
            resid,
            frozen,
        },
        "thin" => Variant {
            thin: Some(thin(&p[1..])),
            relative: 0.0,
            resid,
            frozen,
        },
        "thinrel" => Variant {
            thin: Some(thin(&p[1..5])),
            relative: p[5].parse().unwrap(),
            resid,
            frozen,
        },
        other => panic!("unknown variant {other}"),
    }
}

/// The rows of `shots` to keep: at most `per_cell` per `cell`-unit cell and `cap` in all,
/// strongest first (or in a deterministic pseudo-random order). Each path keeps its own noise
/// model; a path left without rows has none. `strengths[s][r]` replaces the observed acceleration
/// as the row preference when given.
pub(super) fn thin_shots(
    shots: &[Shot],
    cell: f64,
    per_cell: usize,
    cap: usize,
    strong: bool,
    strengths: Option<&[Vec<f64>]>,
) -> Vec<Shot> {
    // (shot, row, strength, cell)
    let mut candidates: Vec<(usize, usize, f64, (i64, i64))> = Vec::new();
    for (s, shot) in shots.iter().enumerate() {
        let Some(path) = &shot.path else { continue };
        for r in 0..path.row_count() {
            let c = path.center(r);
            candidates.push((
                s,
                r,
                strengths.map_or_else(|| path.strength(r), |v| v[s][r]),
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
    shots
        .iter()
        .zip(kept)
        .map(|(shot, mut keep)| {
            keep.sort_unstable();
            Shot {
                obs: shot.obs.clone(),
                contact: None,
                path: shot.path.as_ref().and_then(|p| p.thinned(&keep)),
            }
        })
        .collect()
}

/// The shots of the search stage: relative noise term and/or thinned rows.
/// `strengths[s][r]` replaces the observed acceleration as the row preference when given.
pub(super) fn search_shots(
    case: &Case,
    variant: Variant,
    noise: f64,
    strengths: Option<&[Vec<f64>]>,
) -> Vec<Shot> {
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
        shots = thin_shots(&shots, cell, per_cell, cap, strong, strengths);
    }
    shots
}

fn work_done(run: &Run) -> bool {
    run.limits
        .max_evaluations
        .is_some_and(|m| run.stats.evaluations() >= m)
}

pub(super) fn grow(
    joint: &Fit,
    shots: &[Shot],
    modes: &mut Vec<Mode>,
    run: &Run,
    world: WorldParameters,
) {
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

/// Growth with the search rows re-thinned every round by the residual of the current best map
/// (whitened residual magnitude under the exact noise, residual-only evaluations not counted).
/// The row set changes per round, so evidence is only compared within a round.
fn grow_resid(
    case: &Case,
    variant: Variant,
    noise: f64,
    exact: &Fit,
    modes: &mut Vec<Mode>,
    run: &Run,
    world: WorldParameters,
) {
    let never = AtomicBool::new(false);
    for _ in 0..60 {
        let parent = best(modes).clone();
        if parent.planets.len() >= run.limits.max_planets || work_done(run) {
            break;
        }
        let theta = theta_of_planets(&parent.planets);
        let (res, _) = exact.evaluate(&theta, false, 0, 1).unwrap();
        let mut offset = 0;
        let strengths: Vec<Vec<f64>> = case
            .shots
            .iter()
            .map(|shot| {
                let r = shot.path.as_ref().map_or(0, |p| p.row_count());
                let v = (0..r)
                    .map(|i| res[offset + i].hypot(res[offset + r + i]))
                    .collect();
                offset += 2 * r;
                v
            })
            .collect();
        let search = search_shots(case, variant, noise, Some(&strengths));
        let joint = Fit::new(&search, world, &never, false, vec![]).with_stats(run.stats);
        let here = mode_at(&joint, &theta).unwrap();
        let children = births(&search, &joint, &here, world, &never, run);
        let improved = children
            .first()
            .is_some_and(|c| c.log_evidence > here.log_evidence);
        *modes = children;
        modes.push(here);
        if !improved {
            break;
        }
    }
}

/// Freeze-and-continue growth: the planets found so far are fixed field (never refitted); each
/// round thins the full rows by the raw residual under them, seeds from the unexplained windows
/// of the thinned rows, fits one new planet on them and freezes it if the (thinned) evidence
/// improves. Returns the frozen map.
fn grow_frozen(
    case: &Case,
    variant: Variant,
    noise: f64,
    mut frozen: Vec<Planet>,
    run: &Run,
    world: WorldParameters,
) -> Vec<Planet> {
    let never = AtomicBool::new(false);
    for _ in 0..40 {
        if frozen.len() >= run.limits.max_planets || work_done(run) {
            break;
        }
        let full = Fit::new(&case.shots, world, &never, false, frozen.clone());
        let strengths: Vec<Vec<f64>> = case
            .shots
            .iter()
            .enumerate()
            .map(|(index, shot)| {
                let Some(path) = &shot.path else {
                    return Vec::new();
                };
                let field = full.field(index, path, &[]);
                let r = path.row_count();
                let mut raw = [vec![0.0; r], vec![0.0; r]];
                for axis in 0..2 {
                    path.raw_residual(axis, &field[axis], &mut raw[axis]);
                }
                (0..r).map(|i| raw[0][i].hypot(raw[1][i])).collect()
            })
            .collect();
        let search = search_shots(case, variant, noise, Some(&strengths));
        let joint = Fit::new(&search, world, &never, false, frozen.clone()).with_stats(run.stats);
        let seeds = seeds(&joint, &[], world);
        if seeds.is_empty() {
            break;
        }
        let left = run
            .limits
            .max_evaluations
            .map_or(u64::MAX, |m| m.saturating_sub(run.stats.evaluations()));
        let budget = Budget {
            evaluations: left.div_ceil(seeds.len() as u64).min(1500),
            deadline: None,
            threads: (cores() / seeds.len()).max(1),
        };
        let flat = mode_at(&joint, &[]).unwrap();
        let mut modes: Vec<Mode> =
            par_map(&seeds, cores(), |seed| refit(&joint, &[*seed], &budget))
                .into_iter()
                .flatten()
                .collect();
        modes.sort_by(|a, b| b.log_evidence.total_cmp(&a.log_evidence));
        match modes.first() {
            Some(m) if m.log_evidence > flat.log_evidence => frozen.push(m.planets[0].clone()),
            _ => break,
        }
    }
    frozen
}

pub(super) fn prune(joint: &Fit, modes: &mut Vec<Mode>, run: &Run, depth: usize) {
    let mut parent = best(modes).clone();
    for _ in 0..depth {
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

pub(super) struct Score {
    pub k: usize,
    pub recall2: usize,
    pub recall15: usize,
    pub spurious: usize,
    pub ratio: f64,
}

pub(super) fn score(theta: &[[f64; 3]], truth: &[[f64; 3]], exact: &Fit, rows: usize) -> Score {
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
            // `a+b`: search stages in turn; each continues growing from the previous stage's map.
            let stages: Vec<Variant> = spec.split('+').map(parse).collect();
            let variant = stages[0];
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
            let search = search_shots(&case, variant, noise, None);
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
                    prune(&exact, &mut modes, &run, 6);
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
                if variant.resid {
                    grow_resid(&case, variant, noise, &exact_fit, &mut smodes, &run1, world);
                } else {
                    grow(&sjoint, &search, &mut smodes, &run1, world);
                }
                for later in &stages[1..] {
                    let theta = theta_of_planets(&best(&smodes).planets);
                    let shots = search_shots(&case, *later, noise, None);
                    let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats);
                    let b = Budget {
                        evaluations: u64::MAX,
                        deadline: None,
                        threads: cores(),
                    };
                    // Refit the map on this stage's rows, then grow on them.
                    smodes = vec![mode_at(&joint, &theta).unwrap()];
                    smodes.extend(refit(&joint, &theta, &b));
                    grow(&joint, &shots, &mut smodes, &run1, world);
                }
                let found = theta_of_planets(&best(&smodes).planets);
                let found = if variant.frozen {
                    let s = score(&found, &truth, &exact_fit, rows);
                    println!(
                        "   stage1: K {} rec2 {} rec15 {} spur {} ratio {:.3e} evals {}",
                        s.k,
                        s.recall2,
                        s.recall15,
                        s.spurious,
                        s.ratio,
                        stats.evaluations()
                    );
                    let planets = best(&smodes).planets.clone();
                    let l = limits(budget);
                    let run = Run {
                        limits: &l,
                        stats: &stats,
                    };
                    theta_of_planets(&grow_frozen(&case, variant, noise, planets, &run, world))
                } else {
                    found
                };
                if variant.frozen {
                    smodes = vec![mode_at(&exact, &found).unwrap()];
                }
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
                                resid: false,
                                frozen: false,
                            },
                            noise,
                            None,
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
                    let show = |label: &str, modes: &[Mode]| {
                        let s = score(
                            &theta_of_planets(&best(modes).planets),
                            &truth,
                            &exact_fit,
                            rows,
                        );
                        println!(
                            "   {label}: K {} rec2 {} rec15 {} spur {} ratio {:.3e} evals {}",
                            s.k,
                            s.recall2,
                            s.recall15,
                            s.spurious,
                            s.ratio,
                            stats.evaluations()
                        );
                    };
                    show("after exact refit", &modes);
                    grow(&exact, &case.shots, &mut modes, &run2, world);
                    show("after exact growth", &modes);
                    prune(&exact, &mut modes, &run2, 6);
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
