//! E9c: a relative-error allowance as part of the likelihood after the thinned search stage, and
//! drift-safe polish variants (`docs/estimator-experiments.md`). Stage 1 is the E9 cascade
//! (thinned rows, relative sd growing down); it runs once per world. Every polish program then
//! starts from that same map with a fresh evaluation counter, so programs compare at equal
//! accounting. Truth scores only. Run with
//! `cargo test --release --lib diagnose_polish -- --ignored --nocapture --test-threads=1`.
//!
//! Environment: `STAGES` (cascade spec, as `VARIANTS` of `thinning`), `STAGE1` (evaluation cap of
//! the cascade, default 28000), `POLISH_BUDGET` (evaluations per polish program, default 12000),
//! `POLISH` (comma list of programs), `NOISE`, plus `SEEDS`/`LIVE`/`KS`/`PATHS` of `cases`.
//!
//! A program is steps joined by `+`. A step is a relative sd level on full rows (`0` is the exact
//! likelihood) followed by flags: `g` grow (exact births under that level), `d` deaths,
//! `G<units>` guard (accept the step only if the exact chi2 falls and no start planet moves more
//! than the given distance, else keep the start), `C<chi2/row>` with a guard: accept regardless of the displacement when the result's exact chi2 per
//! row is at most that (a polish that explains the data is not drift), `S<chi2/row>` skip the step when the start's
//! exact chi2 per row is already at most that. Every step first refits the map on its rows, and
//! the evidence of its births and deaths uses its own level. A `t<cap>:` prefix thins the step's
//! rows (cell 14, 2 per cell, strongest first). Example: `0.005gd+0.001gd+0`, `t6000:0.002gd+0`.
use super::thinning::{Variant, grow, parse, prune, score, search_shots};
use super::*;
use crate::estimator::fit::Budget;
use crate::estimator::search::{Mode, refit};
use crate::estimator::work::cores;

struct Step {
    /// Thinned rows (cell 14, 2 per cell, strongest first) up to this many; `None` is all rows.
    cap: Option<usize>,
    level: f64,
    grow: bool,
    deaths: bool,
    guard: Option<f64>,
    /// Guard override: accept anyway when the exact chi2 per row of the result is at most this.
    good_chi2: Option<f64>,
    skip_below: Option<f64>,
}

fn parse_program(spec: &str) -> Vec<Step> {
    spec.split('+')
        .map(|s| {
            let (cap, s) = match s.strip_prefix('t') {
                Some(r) => {
                    let (c, rest) = r.split_once(':').unwrap();
                    (Some(c.parse().unwrap()), rest)
                }
                None => (None, s),
            };
            let end = s
                .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                .unwrap_or(s.len());
            let mut step = Step {
                cap,
                level: s[..end].parse().unwrap(),
                grow: false,
                deaths: false,
                guard: None,
                good_chi2: None,
                skip_below: None,
            };
            let mut rest = &s[end..];
            while let Some(c) = rest.chars().next() {
                rest = &rest[1..];
                let number = |rest: &mut &str| {
                    let e = rest
                        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
                        .unwrap_or(rest.len());
                    let v: f64 = rest[..e].parse().unwrap();
                    *rest = &rest[e..];
                    v
                };
                match c {
                    'g' => step.grow = true,
                    'd' => step.deaths = true,
                    'G' => step.guard = Some(number(&mut rest)),
                    'C' => step.good_chi2 = Some(number(&mut rest)),
                    'S' => step.skip_below = Some(number(&mut rest)),
                    other => panic!("unknown flag {other}"),
                }
            }
            step
        })
        .collect()
}

fn relative_shots(case: &Case, cap: Option<usize>, level: f64, noise: f64) -> Vec<Shot> {
    let variant = Variant {
        thin: cap.map(|c| (14.0, 2, c, true)),
        relative: level,
        resid: false,
        frozen: false,
    };
    search_shots(case, variant, noise, None)
}

fn acceleration(theta: &[[f64; 3]], p: [f64; 2]) -> [f64; 2] {
    let mut a = [0.0; 2];
    for t in theta {
        let mass = planet_at(*t).mass;
        let (dx, dy) = (t[0] - p[0], t[1] - p[1]);
        let d2 = (dx * dx + dy * dy).max(1e-12);
        let d = d2.sqrt();
        a[0] += dx / d * mass / d2;
        a[1] += dy / d * mass / d2;
    }
    a
}

/// RMS acceleration error of `theta` against `truth` on a grid, over grid points within 30 units
/// of a visited row (explored) and the others (unexplored); points inside a true planet are left
/// out.
fn field_rmse(case: &Case, theta: &[[f64; 3]], truth: &[[f64; 3]]) -> (f64, f64) {
    let world = WorldParameters::default();
    let visited: Vec<[f64; 2]> = case
        .shots
        .iter()
        .filter_map(|s| s.path.as_ref())
        .flat_map(|p| (0..p.row_count()).map(|r| p.center(r)))
        .collect();
    let mut sums = [(0.0, 0usize); 2];
    for i in 0..(world.battlefield_width / 20.0) as usize {
        for j in 0..(world.battlefield_height / 20.0) as usize {
            let p = [10.0 + 20.0 * i as f64, 10.0 + 20.0 * j as f64];
            if truth
                .iter()
                .any(|t| (t[0] - p[0]).hypot(t[1] - p[1]) < t[2])
            {
                continue;
            }
            let explored = visited
                .iter()
                .any(|v| (v[0] - p[0]).hypot(v[1] - p[1]) < 30.0);
            let (a, b) = (acceleration(theta, p), acceleration(truth, p));
            let e = (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2);
            let s = &mut sums[usize::from(!explored)];
            s.0 += e;
            s.1 += 1;
        }
    }
    let rms = |s: (f64, usize)| (s.0 / s.1.max(1) as f64).sqrt();
    (rms(sums[0]), rms(sums[1]))
}

/// Median distance from each true planet to its nearest fitted one, and the mean of those below 15.
fn position_error(theta: &[[f64; 3]], truth: &[[f64; 3]]) -> (f64, f64) {
    let mut d: Vec<f64> = truth
        .iter()
        .map(|p| {
            theta
                .iter()
                .map(|t| (t[0] - p[0]).hypot(t[1] - p[1]))
                .fold(f64::INFINITY, f64::min)
        })
        .collect();
    d.sort_by(|a, b| a.total_cmp(b));
    let close: Vec<f64> = d.iter().copied().filter(|v| *v < 15.0).collect();
    (
        d[d.len() / 2],
        close.iter().sum::<f64>() / close.len().max(1) as f64,
    )
}

/// Largest distance from a start planet to the nearest planet of `after`.
fn displacement(start: &[[f64; 3]], after: &[[f64; 3]]) -> f64 {
    start
        .iter()
        .map(|s| {
            after
                .iter()
                .map(|t| (t[0] - s[0]).hypot(t[1] - s[1]))
                .fold(f64::INFINITY, f64::min)
        })
        .fold(0.0, f64::max)
}

#[test]
#[ignore]
fn diagnose_polish() {
    let never = AtomicBool::new(false);
    let world = WorldParameters::default();
    let noise: f64 = std::env::var("NOISE").map_or(0.0, |v| v.parse().unwrap());
    let stage1_cap: u64 = std::env::var("STAGE1").map_or(28_000, |v| v.parse().unwrap());
    let polish_budget: u64 = std::env::var("POLISH_BUDGET").map_or(12_000, |v| v.parse().unwrap());
    let cascade = std::env::var("STAGES").unwrap_or_else(|_| {
        "thinrel:14:2:500:strong:0.1+thinrel:14:2:1000:strong:0.02+thinrel:14:2:2000:strong:0.005"
            .into()
    });
    let programs: Vec<String> = std::env::var("POLISH")
        .unwrap_or_else(|_| "0gd".into())
        .split(',')
        .map(String::from)
        .collect();
    let ceilings = PartCeilings::default();
    for case in cases(noise) {
        let rows = rows(&case);
        let truth = theta_of_planets(&case.truth);
        let exact_fit = Fit::new(&case.shots, world, &never, false, vec![]);
        println!(
            "== {} rows {rows} nodes {} K_true {} truth chi2/row {:.2}",
            case.name,
            nodes(&case),
            truth.len(),
            chi2_per_row(&exact_fit, &truth, rows)
        );
        let supported_ids = supported(&exact_fit, &truth);
        let supported_count = supported_ids.len();
        let limits = |max: u64| Limits {
            max_planets: 32,
            deadline: Instant::now() + Duration::from_secs(36000),
            max_evaluations: Some(max),
            parallel_evaluations: true,
            ceilings: &ceilings,
        };
        let report = |label: &str, theta: &[[f64; 3]], evals: u64| {
            let s = score(theta, &truth, &exact_fit, rows);
            let (med, mean) = position_error(theta, &truth);
            let (explored, unexplored) = field_rmse(&case, theta, &truth);
            println!(
                "   {label}: K {} rec2 {} (supported {}/{supported_count}) rec15 {} spur {} ratio {:.3e} chi2/row {:.2} pos med {med:.2} mean<15 {mean:.2} field explored {explored:.2e} unexplored {unexplored:.2e} evals {evals}",
                s.k,
                s.recall2,
                supported_ids
                    .iter()
                    .filter(|&&i| {
                        theta
                            .iter()
                            .any(|t| (t[0] - truth[i][0]).hypot(t[1] - truth[i][1]) < 2.0)
                    })
                    .count(),
                s.recall15,
                s.spurious,
                s.ratio,
                chi2_per_row(&exact_fit, theta, rows),
            );
        };

        // Stage 1: the E9 cascade, once.
        let stats1 = Stats::default();
        let stages: Vec<Variant> = cascade.split('+').map(parse).collect();
        let l1 = limits(stage1_cap);
        let run1 = Run {
            limits: &l1,
            stats: &stats1,
        };
        let mut equiv1 = 0.0;
        let mut smodes: Vec<Mode> = Vec::new();
        for (i, stage) in stages.iter().enumerate() {
            let shots = search_shots(&case, *stage, noise, None);
            let cost: usize = shots
                .iter()
                .filter_map(|s| s.path.as_ref())
                .map(|p| p.nodes.len())
                .sum();
            let before = stats1.evaluations();
            let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats1);
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
            grow(&joint, &shots, &mut smodes, &run1, world);
            equiv1 += (stats1.evaluations() - before) as f64 * cost as f64 / nodes(&case) as f64;
        }
        let start = theta_of_planets(&best(&smodes).planets);
        let evals1 = stats1.evaluations();
        report(
            &format!("stage1 (evals {evals1}, equiv {equiv1:.0})"),
            &start,
            evals1,
        );

        for program in &programs {
            let steps = parse_program(program);
            let stats = Stats::default();
            let l = limits(polish_budget);
            let run = Run {
                limits: &l,
                stats: &stats,
            };
            let mut theta = start.clone();
            let mut equiv = 0.0;
            for step in &steps {
                let left = polish_budget.saturating_sub(stats.evaluations());
                if let Some(t) = step.skip_below {
                    let c = chi2_per_row(&exact_fit, &theta, rows);
                    if c <= t {
                        println!(
                            "   [{program}] step {} skipped: chi2/row {c:.2}",
                            step.level
                        );
                        continue;
                    }
                }
                let shots = relative_shots(&case, step.cap, step.level, noise);
                let cost: usize = shots
                    .iter()
                    .filter_map(|s| s.path.as_ref())
                    .map(|p| p.nodes.len())
                    .sum();
                let evals_before = stats.evaluations();
                let joint = Fit::new(&shots, world, &never, false, vec![]).with_stats(&stats);
                let b = Budget {
                    evaluations: left.max(500),
                    deadline: None,
                    threads: cores(),
                };
                let mut modes = vec![mode_at(&joint, &theta).unwrap()];
                modes.extend(refit(&joint, &theta, &b));
                if step.grow {
                    grow(&joint, &shots, &mut modes, &run, world);
                }
                if step.deaths {
                    prune(&joint, &mut modes, &run);
                }
                equiv +=
                    (stats.evaluations() - evals_before) as f64 * cost as f64 / nodes(&case) as f64;
                let result = theta_of_planets(&best(&modes).planets);
                let mut note = String::new();
                let accepted = match step.guard {
                    Some(limit) => {
                        let (before, after) = (
                            exact_fit.chi2(&theta).unwrap(),
                            exact_fit.chi2(&result).unwrap(),
                        );
                        let moved = displacement(&theta, &result);
                        note =
                            format!(" guard: chi2 {before:.3e} -> {after:.3e}, moved {moved:.1}");
                        after < before
                            && (moved < limit
                                || step.good_chi2.is_some_and(|t| after / rows as f64 <= t))
                    }
                    None => true,
                };
                if accepted {
                    theta = result;
                }
                report(
                    &format!(
                        "[{program}] step {}{}{}{note}",
                        step.level,
                        if accepted { "" } else { " REJECTED" },
                        ""
                    ),
                    &theta,
                    evals1 + stats.evaluations(),
                );
            }
            let s = score(&theta, &truth, &exact_fit, rows);
            let (med, mean) = position_error(&theta, &truth);
            let (explored, unexplored) = field_rmse(&case, &theta, &truth);
            println!(
                "RESULT {} | {program} | K {} rec2 {}/{} rec15 {} spur {} ratio {:.3e} | pos med {med:.2} mean<15 {mean:.2} | field explored {explored:.2e} unexplored {unexplored:.2e} | evals {} (stage1 {evals1}, equiv {equiv1:.0}, polish {}, polish equiv {equiv:.0})",
                case.name,
                s.k,
                s.recall2,
                truth.len(),
                s.recall15,
                s.spurious,
                s.ratio,
                evals1 + stats.evaluations(),
                stats.evaluations(),
            );
        }
    }
}
