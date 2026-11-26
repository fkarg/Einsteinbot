# Likelihood value probe (P3), 2026-11-26

Question: how much accuracy does full-path simulation add to planet estimation, versus comparing the model's gravity field with the field implied by the observed path? Offline probe only: `src/bin/likelihood_value_probe.rs` (`cargo run --bin likelihood_value_probe -- <selfcheck|e1|isolation|flat|paths|multi|captures|timing> [--quick]`). The live estimator is unchanged. Truth is used for scoring and for generating synthetic data, never as fit input (one isolation run, "true substeps", feeds true substep positions to W on purpose).

Grading: everything below is **observed** unless marked *inferred*. Every table is a **single run** with a fixed seed (seeds in headings); no repeat runs.

## Methods (same parameterization, optimizer, start set and evidence rule; only the residual differs)

All fit planets `(x, y, r)` with mass `r^3/10`, `r` in 20-40, logistic bounds, the estimator's interior-sample penalty, multi-start Levenberg-Marquardt with finite-difference Jacobians (the same crate), truth-free starts (acceleration-peak proposals plus a 6x4 grid, at most 40), greedy K growth, Laplace evidence as in `estimator/fit.rs` (`-chi2/2 - Occam - K ln 20`). Contacts are not used.

- **S** full-path velocity-Verlet simulation from the launch state (copy of the engine arithmetic; verified equal to `simulation.rs` to 0.0 on a test path). Residuals are position errors divided by the f32 rounding sd (not the live estimator's 0.5 floor).
- **W(n)** no dynamics. The engine scheme gives `x[m+1]-2x[m]+x[m-1] = dt^2 a[m]` exactly, so a tick second difference is `dt^2` times a triangle-weighted (`25-|i|`) sum of substep accelerations, and a window of `n` ticks is a trapezoid-weighted sum. Observed value: `D[t+n]-D[t]` (telescoped first differences: noise does not grow with `n`). Predicted value: that kernel applied to the model field at positions on the observed path. Between ticks the path is a cubic Hermite interpolant with 4th-order central-difference tangents (exact launch velocity at tick 0, 2nd-order at the ends). One extra row uses the known launch velocity (`p1-p0-v0`, same kernel idea). Windows start every `max(1, n/2)` ticks (overlapping); noise covariance is exact for f32 rounding (uniform, sd `ulp/sqrt(12)` per coordinate), including overlap correlations (banded Cholesky whitening). `W(n)/q` evaluates the field only every `q` substeps and interpolates acceleration linearly in between (q=1 is the exact kernel).
- **A(m)** abbreviated simulation: from each observed tick state integrate `m` ticks (25m substeps) and compare. Non-overlapping windows. Seed velocity: exact launch velocity at tick 0, otherwise the 4th-order central difference of observed ticks (model-free). Residual sd includes seed position and velocity noise, treated as independent across windows (approximation).
- **Sref** the existing estimator (public API, K<=1, its own 0.5 noise floor and starts) for reference.

Kernel check (`selfcheck`, also a unit test): W on true substep positions of a two-planet flyby reproduces the observed windows to 1.4e-6 whitened units (n=1,2,8,32); on Hermite-interpolated f64 ticks the largest whitened residual is 0.6 on that path.

Metrics: centre error, radius error, relative mass error (median over fits; p90 for centre), "recov%" = centre within 10 units, field vector RMSE on the 20-unit grid explored/unexplored via `field_score` (flat reference in parentheses), z-scores of `(estimate-truth)/sd` from `J^T J` at the optimum (x, y, r pooled; ideal median |z| 0.67 and 95% below 2; the second percentage uses the covariance inflated by `max(1, chi2/dof)`), search-fail% (truth has lower chi2 than the fit found: a search failure, not a likelihood failure), median evals and wall time per fit.

## E1: K=1, 256 single-path fits (32 worlds x 8 paths, 6 aimed + 2 blind; seed 1001), f32 data, K known

| method | recov% | centre med | centre p90 | dR med | expl RMSE med | median abs(z) / <2 |
|---|---:|---:|---:|---:|---:|---|
| S | 98 | 3.7e-3 | 0.86 | 1.9e-4 | 2.5e-7 | 2.14 / 47% |
| W1 | 98 | 3.2e-3 | 0.46 | 1.6e-4 | 1.7e-7 | 1.06 / 74% |
| W2 | 99 | 7.4e-3 | 1.3 | 3.3e-4 | 3.6e-7 | 0.85 / 77% |
| W4 | 94 | 0.024 | 5.4 | 1.1e-3 | 1.5e-6 | 0.76 / 86% |
| W8 | 89 | 0.045 | 10.3 | 1.9e-3 | 2.3e-6 | 0.80 / 90% |
| W16 | 84 | 0.10 | 43.8 | 4.0e-3 | 5.9e-6 | 0.71 / 92% |
| W32 | 82 | 0.34 | 43.0 | 0.015 | 1.2e-5 | 0.71 / 92% |
| W8/q5 | 89 | 0.081 | 10.3 | 3.7e-3 | 4.2e-6 | 1.21 / 68% |
| W8/q25 | 89 | 0.59 | 10.9 | 0.027 | 1.2e-5 | 2.08 / 49% |
| A1 | 80 | 2.8 | 19.1 | 0.10 | 3.0e-5 | 1.90 / 50% |
| A2 | 89 | 0.37 | 10.4 | 0.018 | 6.7e-6 | 0.78 / 66% |
| A4 | 93 | 0.23 | 6.0 | 0.010 | 7.6e-6 | 1.63 / 58% |
| A8 | 93 | 0.17 | 6.4 | 5.7e-3 | 5.7e-6 | 1.80 / 54% |
| Sref (existing) | 69 | 1.8e-3 | 0.135 | 9.3e-5 | 9.8e-7 | z not comparable |

Flat-field explored RMSE median is 0.018. Per-stratum tables (distance, speed, length, radius; all methods) are produced by `e1`; the ones that decide things:

Centre error median by distance of the path to the planet centre (recov% in brackets):

| band (n fits) | S | W1 | W4 | W8 | W16 | A2 | A4 | A8 |
|---|---|---|---|---|---|---|---|---|
| near <40 (27) | 2.5e-4 (100) | 2.0e-3 (100) | 5.3e-3 | 3.1e-3 | 3.6e-3 | 0.27 (96) | 0.036 | 7.3e-3 |
| mid 40-150 (53) | 3.2e-4 (100) | 1.0e-3 (100) | 2.9e-3 | 3.6e-3 | 7.5e-3 | 0.34 | 0.068 | 0.033 |
| far 150-400 (71) | 3.0e-3 (100) | 7.9e-4 (100) | 0.012 | 0.020 | 0.047 (97) | 0.077 | 0.065 | 0.054 |
| very far >400 (105) | 0.064 (95) | 0.053 (96) | 0.41 (86) | 0.85 (74) | 3.4 (63) | 1.6 (75) | 1.9 (83) | 1.4 (82) |

z calibration by band (median abs z): W1 near 19.5 (0% below 2) but 0.69-0.79 far; W8 near 0.97; S 1.7-2.7 everywhere. Speed, length and radius strata show the same ordering (S and W1 best, longer W windows lose on short and weak paths: len <40 W16 67% recov vs S 96%); no stratum reverses the distance picture. Near passes by speed were not cross-tabulated.

Detection (K=1 truth, evidence-selected K, 256 fits per method): every method admits the planet in 100% of fits in every distance band, including >400 units.

## E2: isolation (24 worlds x <=6 paths, single path, K=1 known; seed 2002)

| data | S | W1 | W4 | W16 | A1 | A4 |
|---|---|---|---|---|---|---|
| f32 (centre med) | 5.8e-3 | 4.1e-3 | 0.032 | 0.25 | 1.9 | 0.29 |
| f64 unrounded | 2.3e-11 | 1.5e-7 | 6.4e-7 | 4.2e-6 | 1.6 | 8.9e-3 |
| f32 + true substeps in W | - | 1.9e-3 | 0.026 | 0.25 | - | - |
| near passes only (12 fits): f32 | 1.9e-4 | 4.1e-3 | 0.010 | 0.011 | 7.5 | 0.051 |
| near, f64 | 1.4e-12 | 4.0e-3 | 0.011 | 7.2e-5 | 7.5 | 0.042 |
| near, f32 + true substeps | - | 1.0e-4 | 7.1e-4 | 8.3e-3 | - | - |

Reading: (a) f64 removes the error of S and of W on far and mid paths (rounding noise limited those), but not the near-pass error of W1/W4 (4e-3 unchanged) or the seed-velocity error of A (A1 1.6 unchanged); (b) true substep positions cut W1's near-pass error from 4.1e-3 to 1.0e-4, so the near-pass loss of W is interpolation; (c) in f64 a long window is better near passes (W16 7e-5 vs W1 4e-3: Hermite errors average out over a window) but in f32 it is worse overall (it keeps only about half the information at stride n/2 *inferred*). Window length trades interpolation robustness against rounding-noise information.

## E3: flat worlds, K by evidence (32 worlds x 4 paths = 128 fits per cell; seed 3003)

With the true noise model (f32 rounding only) no method admitted a planet: 0 of 128 for S, Sref, W1/16/32/4, A1/4/8. With extra Gaussian position noise N(0, sigma) added:

| extra noise | S | W1 | W4 | W16 | A1 | A4 | A8 |
|---|---:|---:|---:|---:|---:|---:|---:|
| 0.01, noise told to the likelihood | 0 | 0 | 0 | 0 | 0 | 3.9 | 3.9 |
| 0.01, untold | 9.4 | 3.9 | 46.9 | 57.0 | 75.8 | 79.7 | 65.6 |
| 0.1, told | 0 | 0 | 0 | 0 | 0 | 4.7 | 10.2 |
| 0.1, untold | 32.8 | 28.9 | 88.3 | 94.5 | 89.8 | 89.8 | 86.7 |

(percent of fits that admit a planet). Unmodeled noise is the only failure mode, and S and W1 are the most robust; A and long W windows are fragile.

## E4: paths needed (16 worlds, joint fit of the first n paths, K=1 known; seed 4004; median centre error)

| paths | S | W4 | W16 | A2 | A8 |
|---|---|---|---|---|---|
| 1 | 0.054 | 0.14 | 0.99 | 0.84 | 0.43 |
| 2 | 4.0e-4 | 5.8e-3 | 0.021 | 0.22 | 0.055 |
| 4 | 6.0e-4 | 4.4e-3 | 5.1e-3 | 0.14 | 0.021 |
| 8 | 1.0e-4 | 9.6e-4 | 1.4e-3 | 0.29 | 0.012 |

All methods are below 1 unit median after one or two paths; median below 0.01 needs 2 paths for S and W4, 4 for W16, and is not reached by A2 or A8 within 8. S's z-scores worsen with more paths (median abs z 1.6 at one path, 14 at eight, 12% below 2); W16 stays near 1.

## E5: known K=2, K=3 (12 worlds each, 8 aimed paths, greedy K; seeds 5002, 5003)

Centre median: K=2 S 3.9e-4, W8 2.5e-3, W16 2.5e-3, A4 0.072 (all 12/12 worlds recovered). K=3: S 7.2e-4, W8 4.6e-3, W16 0.014, A4 0.20; S and A4 12/12 worlds recovered, W8 and W16 11/12 (one greedy search failure each: the truth had lower chi2 than the fit, so a start-set problem, not a likelihood problem). S's z calibration is bad here too (median abs 9-10, 11-15% below 2), W8 0.9-1.5, A4 10-28.

## E6: disclosed-truth captures (hashes below; first <=8 paths of the epoch with most paths; K by evidence up to 4)

Explored / unexplored field RMSE (flat reference in the heading row). Learning scenarios are synthetic (the bot's own simulator); 44 and 49 are live captures with 24 planets.

| capture (truth K, paths) | flat | S | W4 | W16 | A2 | A8 | Sref |
|---|---|---|---|---|---|---|---|
| learn 2 (1, 2) | 0.140 / 0.138 | 6e-9 / 2e-8 (K=3) | 4e-7 / 1e-6 (1) | 9e-7 / 3e-6 (1) | 2e-4 / 1e-3 (2) | 5e-5 / 3e-4 (2) | 4e-9 / 9e-9 (1) |
| learn 6 (1, 2) | 0.139 / 0.138 | 2e-8 / 2e-7 (K=3) | 1e-7 / 2e-7 (1) | 8e-7 / 3e-6 (1) | 1e-4 / 9e-4 (2) | 4e-5 / 4e-4 (3) | 3e-9 / 7e-9 (1) |
| learn 10 (1, 4) | 0.246 / 0.135 | 5e-8 / 5e-8 (1) | 1e-6 / 9e-7 (1) | 1e-6 / 1e-6 (1) | 1.4e-3 / 1.8e-3 (3) | 5e-5 / 7e-5 (3) | 7e-8 / 7e-8 (1) |
| learn 12 (2, 3) | 0.202 / 0.217 | 4e-8 / 3e-7 (K=4) | 4e-6 / 5e-6 (2) | 2e-6 / 2e-6 (2) | 2e-3 / 2e-3 (4) | 4e-5 / 3e-5 (2) | 0.190 / 1.73 (1) |
| learn 18 (24, 8) | 0.623 / 0.621 | 0.651 / 17.4 (4) | 0.653 / 3.44 (4) | 0.652 / 2.43 (4) | 0.576 / 2.53 (4) | 0.608 / 5.57 (4) | 0.628 / 2.28 (1) |
| live 44 (24, 8) | 0.719 / 0.569 | 0.653 / 2.70 (4) | 0.608 / 11.3 (4) | 1.94 / 4.07 (4) | 0.502 / 2.33 (4) | 0.589 / 1.99 (4) | 0.741 / 1.23 (1) |
| live 49 (24, 8) | 0.595 / 0.613 | 1.02 / 4.32 (4) | 0.488 / 2.94 (4) | 0.749 / 5.46 (4) | 0.395 / 1.26 (4) | 0.437 / 6.01 (4) | 5.43 / 0.653 (1) |

(K selected in brackets.) On one-planet and two-planet synthetic captures all of S, W, Sref are near exact (the paths come from the same simulator, so this only shows inference works under it); S over-admits planets there (K=3 on a one-planet world with 2 paths; K=4 on a two-planet world), W4/W16 chose the true K. On the 24-planet captures (K capped at 4) no method beats flat reliably in the explored area and every method worsens or fails to improve the unexplored area (S worst on learn 18: 17.4 vs 0.62 flat). **This says nothing about which likelihood is better for multi-planet worlds**: K<=4 is misspecified for 24 planets. Whether the live engine matches the simulator at this noise level is not tested here.

## E7: wall time (release build, one thread, one 106-tick path at speed 7.1 passing 28 units from the centre; single run)

| method | per eval K=1 (us) | per eval K=3 (us) | K=1 fit all starts (ms, ~2.5k evals) |
|---|---:|---:|---:|
| S | 99.8 | 82.3 | 160 |
| W1..W32 (exact kernel) | 9.9-12.2 | 16.1-18.7 | 24-36 |
| W8/q5 | 2.4 | 4.0 | 8 |
| W8/q25 | 1.0 | 1.6 | 4 |
| A1..A8 | 34.5-43.9 | 43.5-46.8 | 64-114 |

Ratio S / W exact: about 10x (K=1), 5x (K=3), 5-7x per full fit. Preparation (interpolation, whitening) is 0.1-0.4 ms, once per path. Timings in E1-E6 tables are dev-profile on 10 contended threads and are not comparable.

## Prediction scorecard (brainstorm P3)

1. **W matches S on far and mid paths: supported for small windows.** Mid: S 3.2e-4 vs W1 1.0e-3; far 150-400: S 3.0e-3 vs W1 7.9e-4; very far: 0.064 vs 0.053; all well below 0.1 unit. For `n>=8` it is not supported on very far paths (W8 74% recov, W16 63% vs S 95%).
2. **W loses on near passes: supported, and the cause is interpolation.** Near: S 2.5e-4 vs W1 2.0e-3 (8x worse; field RMSE 2.7e-6 vs 3.9e-5); with true substeps W1 reaches 1.0e-4 (E2). Absolute loss is about 2e-3 units. W1's near-pass uncertainty is badly overconfident (median abs z 19.5). Untested: the speed>30 qualifier (not cross-tabulated).
3. **A with 1-4 ticks recovers most of the near-pass loss: contradicted** (with model-free seed velocity). A1 near 6.6 units (67% recov), A2 0.27, A4 0.036, A8 7.3e-3, all worse than W1's 2.0e-3; in f64 A1 is still 1.6 (seed error, not rounding). A model-aware seed velocity was not tried.
4. **S's advantage appears only for weak fields: contradicted.** S's clear advantage is on near passes (strong field, 8x). In the weak regime (>400 units) S ties W1 (0.064 vs 0.053) and only beats long windows.
5. **W is at least 20x cheaper per likelihood evaluation: contradicted for the exact kernel, supported only for decimated nodes.** Exact: 10x (K=1), 5x (K=3). With field nodes every 5 substeps 41x/21x, every 25 substeps 100x/51x, but accuracy then falls (W8/q5 near centre 0.021 vs 3.1e-3; W8/q25 0.81; z-scores break).
6. **With f64 positions the gap shrinks: mixed.** Absolute W errors on far and mid paths fall by 4-5 orders (rounding-limited), S's by 9; but the near-pass loss of W does not shrink at all (4e-3 in both), so that part is interpolation, not rounding.

Result against the premise "field comparison is cheaper": it is (5-10x per evaluation, 5-7x per fit with the exact kernel), so nothing here reopens the premise. What the probe did find that was unexpected: with exact f32 noise all methods, including S, are accurate to far below the noise floors the live estimator uses (0.5); the practical gaps are 1e-4 to 1e-1 units. The live estimator's own recov% (69%) is lower than the matched-noise S (98%), which points at its noise floor and start heuristics rather than at simulation.

## Recommendation

- Use the **windowed field comparison (W) as the estimation likelihood** for fitting, weighting and births: with the exact kernel and full rounding covariance it matches S on mid/far paths (n=1-2), is 5-10x cheaper per evaluation, convex-friendly in mass (field is linear in mass; not exploited here: Jacobians were finite differences), has no compounding, was the best calibrated, and was among the two most robust to unmodeled noise (flat worlds with untold noise: W1 3.9%/28.9% false planets, S 9.4%/32.8%, long windows and A far worse).
- **Window length by regime** (the hybrid): small windows (n=1-2) for far and weak paths, where information matters; windows of about 8 for near passes (distance below ~60), where interpolation error dominates and the longer window is calibrated (W8 near: median abs z 0.97, centre 3.1e-3). Alternatively add an interpolation-error variance term near the planet; not tested.
- Keep **short simulation only as an optional polish for near passes** (S is 8x tighter there, 2.5e-4 vs 2e-3), applied to planets whose paths pass within ~40 units, not as the main likelihood. A (abbreviated simulation) is dominated here: 3-4x slower than W, less accurate, worse calibrated. Not recommended.
- Do not use S evidence for K selection without calibration: it admitted extra planets on small one/two-planet captures and its covariances are overconfident (median abs z 2 at one path, 9-14 with several).
- Decide separately, with live-engine evidence, how much model-mismatch noise to add; all conclusions here assume the simulator equals the engine and only f32 rounding perturbs the data.

## Caveats

- Single fixed-seed runs; 32 K=1 worlds (256 single-path fits), 12-24 worlds elsewhere; near-pass strata have 12-27 fits. Medians of errors that span 1e-11 to 1e2 hide the heavy tails reported as p90 and recov%.
- Synthetic data come from the probe's copy of the engine arithmetic (equal to `simulation.rs`); the real engine is double precision with the same scheme per source reading, but live agreement was not measured. 
- Planets are drawn with radius 20-40 and mass `r^3/10`, and the fit uses the same prior; shots are aimed with impact parameters up to 400 plus blind ones, speeds log-uniform 2-50, up to 180 ticks.
- W's launch-velocity row gives it information that S also has; contacts are unused for all methods.
- Hermite interpolation with finite-difference tangents is the simplest choice; better interpolators or a model-aware seed for A could change the near-pass and A results.
- Z-score covariances use finite-difference Jacobians at the optimum; S's overconfidence may come from that (chaotic sensitivity) rather than the likelihood *inferred*.
- Search: greedy K growth from a shared truth-free start set; one K=3 failure each for W8/W16 is a search miss.
- Capture runs: databases are local and mutable. SHA-256 at run time: `einsteinbot.sqlite3` 5c9c0bae8458e82862940e66e8244b6cdb22bae28c7bc6822daee0108f5547ed, `learning-scenarios.sqlite3` b31afed8e553a158e6d59b320c717955adfb0bf5258db68d45dd99a6b724a557 (unchanged since `estimator-baseline.md`). The live captures are the 8-path prefixes of the epoch with most paths; capture 44's flat score (0.719) differs from `estimator-baseline.md` (0.854) because paths and checkpoint differ.
