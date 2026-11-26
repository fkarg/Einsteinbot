# Estimator evaluation: proposed measurement protocol

Status: the objective and priorities are user decisions (2026-11-23). The core grid-based field score and causal paired replay below are implemented as an offline first slice. Geometry, trajectory, distributional scores, offset-grid sensitivity and the remaining choices are proposals or existing separate tools, not a completed scorecard. No threshold is calibrated. The estimator learns from paths; disclosed planet maps are used only by offline scorers, never by fitting, model selection inside the bot, or targeting.

Run `cargo run --bin field_scorecard -- --capture-id 12` to emit JSON lines for one causal replay of the capture's event history, scoring the physical-planet map against flat and oracle references. Each `before_launch` row freezes prior completed paths; each `after_completion` row evaluates the updated belief. `--db-path PATH` chooses another capture and `--max-completed N` limits the total number of distinct accepted trajectories across epochs. An empty planet message is treated as undisclosed unless `--empty-is-flat` is supplied for a run independently known to be a flat world. Rows identify unavailable truth, invalid samples and the shared cell counts. `fit_ms` is cumulative estimator-observation time within the epoch, excludes scoring and rounds each event to microseconds. Finite-only RMSE is explicitly conditional and never substitutes for the complete score when some samples fail. The build uses default NewtonWars world dimensions; do not compare captures with other dimensions under that assumption. This is synchronous offline replay, not the belief actually published during a recorded game. Residual and field-only-source models, and the paired grid/bearing branches that existed only to compare their proposals, were removed on 2026-11-26; git history is the only fallback.

## Objective and scope

The primary estimator target is **accuracy of the gravity field in explored areas**. Also evaluate the whole field and extrapolation into unexplored areas; improved extrapolation is valuable and must remain visible. Planet collision geometry is a separate important target. Uncertainty, trajectories and other state estimates are useful secondary diagnostics. For the downstream targeting and strategy system, shot outcomes and game strength are the ultimate targets; that does not make them the sole estimator score.

Keep separate values rather than hiding field, geometry, calibration and outcome failure in a single weighted number. Measure change after each observation and over real elapsed time, including the age of the latest published map. Performance and stale map age are reported alongside accuracy, not substituted for it.

## Frozen evidence and scoring fields

Compare estimators on the same recorded paths, map epoch, evaluation positions and checkpoints. For a predictive check of a shot, freeze the belief and all fit/selection data **before that shot is launched**, not when it completes. At a reset, clear the snapshot and its explored mask; never combine different map epochs. For a complete manager capture use the established shared-event/session attribution contract. Also report submitted/processed watermarks and age when available: a published stale belief and an offline replay using every completed path are different evaluations.

When valid disclosed truth is unavailable, oracle field and physical-geometry scores are unavailable, not zero. An empty or absent disclosure is not proof of a flat world unless the run configuration establishes that fact. Oracle-based scorer settings must not change estimator input, beliefs or candidate choice.

The scored field is the published belief's maximum-weight map only (v2, 2026-11-26: flat by default, at most one planet in the first slice); retained alternative modes are not scored yet. There is no residual or field-only-source variant. An unavailable belief scores as the flat field and is labelled as such, never as measured zero uncertainty.

## Gravity-field accuracy

For a frozen, common set of valid evaluation locations `x_i`, let `e_i = ||g_est(x_i) - g_truth(x_i)||_2`. The baseline scalar for a stratum is vector RMSE `sqrt(sum_i w_i * e_i² / sum_i w_i)` in acceleration units. Also report median, 90th percentile and maximum vector-error magnitude, number of sampled locations, finite-prediction fraction and any invalid/singular evaluation count. If any required estimate is invalid or non-finite, record a failure and do not produce an apparently good score by silently dropping it. Vector RMSE is `sqrt(2)` times this project's per-coordinate RMSE for the same set of 2-D vectors; name units and normalization explicitly.

**Working spatial design:** use a fixed 20-unit grid, aligned with the existing coverage cells, and score each included cell center with equal weight. This approximates equal-area sampling; clipped edge cells have smaller area. Classify cells from prior timed trajectory segments: an explored cell intersects at least one such segment; reuse the coverage traversal's explicit launch-cell and untimed-terminal exclusions. Clip to the battlefield and exclude oracle planet interiors from *all* models' field samples: actual impacts are scored as geometry/trajectory outcomes instead. Grid-center evaluation is an approximation to an explored area, not an assertion that the center itself was sampled or that nearby gravity is known. A finer-grid sensitivity check and a score evaluated directly at timed path positions can expose grid aliasing and near-surface errors.

At each checkpoint report separately:

- **Explored:** equal weight per explored cell, the primary field metric. Repeated passes and high tick sampling do not multiply a cell's weight. No explored cells means unavailable, with a zero cell count, not a perfect score.
- **Unexplored:** equal weight per remaining valid cell, with its coverage fraction and count. This is the explicit extrapolation score; it is not folded into the primary score or assigned a bonus coefficient yet.
- **Whole field:** equal weight per valid cell, with the same spatial domain for every candidate. Report coverage fraction separately.

The mask is computed from a frozen common observation history, not from a candidate's claimed confidence or choice to abstain; otherwise candidates can improve their score by shrinking what they label explored. If policies acquire different trajectories, report each policy's coverage and score on its own history **and** compare estimators on a shared fixed history/mask; do not confuse exploration strategy with estimation accuracy. The 20-unit coverage grid is a spatial convenience/diversity heuristic, not a posterior, residual support, or full evidence-availability metric. Nonlocal constraints from observed paths can make gravity far from a path identifiable; outside the mask is not proof of uncertainty.

For robustness, add an evaluator-owned offset lattice and report field error in distance bands from the observed timed paths, including directly on-path and nearby-but-not-on-path positions. Freeze its offset and band boundaries before comparing candidates. This checks whether a result depends on alignment with the 20-unit coverage grid, and separates interpolating the observed path from nearby generalization. Distances to straight chords are approximations between timed positions; an oracle re-simulation can be an additional diagnostic but is not assumed to be the actual live path when simulator parity is uncertain.

Absolute vector error is primary; it preserves force direction and magnitude. Optionally report magnitude error and angular error, but angular error is unavailable when either vector is too small for a fixed, declared numerical/noise floor. A relative error or flat-field skill is an additional comparison, not a replacement: for a nonzero flat-field MSE, `skill = 1 - MSE(model)/MSE(flat)`, with higher better. Report the denominator and mark skill unavailable in flat/near-zero cases instead of dividing by zero or an arbitrary tiny value.

Surface-adjacent points can dominate squared error. Keep them in the declared primary domain; report distance-to-truth-surface strata and robust percentiles alongside RMSE instead of quietly clipping errors. Compare sensitivity to grid resolution and within-cell placement. Do not select a surface cutoff, importance weights, normalized scale or threshold after seeing which one favors a candidate.

## Geometry and contacts

Score the estimated **physical collision disks**. For the existing viewer-compatible metric, pair physical disks one-to-one using its current closest-pair-first rule and 80-unit center cutoff; report matched count, missed true disks and unmatched/duplicate estimated disks, center and radius absolute-error distributions, and mass/relative-mass error where meaningful. Explicitly label that greedy matching can miss a better global assignment; do not silently replace the viewer's rule. If maximum-cardinality/minimum-cost matching or GOSPA is adopted later, document the cutoff and units, compare both on ambiguous crowded cases, and coordinate any viewer contract change.

Add a set-level geometry diagnostic such as symmetric-difference area of true and predicted collision disks, including unmatched disks. Score terminal-contact boundary distances and contact association on held-out contacts where classification is available; a likely endpoint is not automatically a confirmed planet. Keep uncertain associations visible. Disclosed geometry is scoring input only.

## Trajectories and outcomes

Replay **fixed, independently launched held-out shots** against the pre-launch belief. Compare predicted positions with observed regular timed samples over declared shared horizons; report the error-versus-horizon curve and number of paths still observable at each horizon, plus per-path coordinate RMSE (existing convention), median and tail across paths, unmatched samples and prediction failures. Give each distinct path equal weight in across-path summaries; many correlated ticks do not make one path many independent observations. The existing `compare_trajectories` returns zero RMSE for zero compared points; an evaluator must not interpret that as a successful prediction.

Separate fixed-horizon no-termination integration, useful for diagnosing gravity, from collision-aware bounded replay, useful for actual flight. Report predicted versus observed termination class and time, first-termination mismatch, and where identifiable the hit/miss and collided-object confusion counts. Do not pad a terminated prediction with fabricated positions or average only successfully predicted prefixes without reporting failures and censoring. For player hits, use only player-state information available at launch and distinguish moving-player/timing errors from gravity or geometry error. Same-simulator synthetic paths test inference under the assumed simulator; also assess disclosed live captures and simulator/engine parity separately.

## Uncertainty and distributions

Do not score a heuristic as a calibrated probability. Evaluate nominal field error and reported support/uncertainty separately; disclose fraction of locations and worlds for which each is available. Complete physical parameter covariance is conditional on the selected physical topology and omits unknown planets and association uncertainty. Approximate or rank-deficient covariance may omit null directions; label it partial and never treat zeros in those directions as certainty or as a complete two-dimensional field distribution.

Once an estimator outputs a defensible probability distribution/ensemble of vector fields, evaluate it at fixed common locations with a proper multivariate score, such as the energy score `mean_j ||G_j-g_truth|| - 0.5 * mean_jk ||G_j-G_k||` (lower is better). This is the score of the finite empirical distribution; when comparing differently sized random ensembles, account for finite-ensemble bias or use the same ensemble size. Report the spatial marginal score, ensemble support/availability and calibration together. For full-rank distributions, evaluate empirical 50%, 90% and 95% predictive-region coverage and sharpness across independent worlds, stratified by explored/unexplored region; a single map's many correlated cells are not independent calibration trials. Aggregate or resample at world/capture level and show sample counts. Joint-field dependence needs a separate diagnostic: pointwise marginal scores alone do not evaluate correlations across locations; a variogram-style score is one possible supplement.

KL divergence requires two specified probability distributions and consistent support; against deterministic point truth and a continuous forecast it is generally not a finite KL. A log density score requires an explicit continuous predictive density and observation/noise model; it can become infinite for a forecast assigning zero density to truth. Normalizing spatial gravity magnitudes into a distribution permits a KL/Jensen–Shannon comparison, but changes the question and discards overall force scale and direction. None of those scores substitutes for vector error, and heuristic alternative weights must not be described as posterior probabilities. Sources: [Gneiting and Raftery, proper scoring rules](https://stat.uw.edu/research/tech-reports/strictly-proper-scoring-rules-prediction-and-estimation); [Scheuerer and Hamill, variogram scores](https://journals.ametsoc.org/doi/10.1175/MWR-D-14-00269.1).

## Falsifiers before relying on the scorecard

- Exact truth model must give only the known numerical/oracle floor; flat/no-source truth must not make normalized scores divide by zero.
- Duplicates and missed planets must be counted.
- Replaying a path at higher point density or repeating it cannot artificially multiply its explored-area weight; changing an estimator's claimed confidence cannot change the fixed evaluation mask.
- A deliberately wrong planet must worsen fixed common-field metrics even if it can claim low support; invalid or singular predictions must surface as failures.
- A prediction that is accurate only after seeing the held-out launch or completion must fail the pre-launch snapshot check. An early collision or unmatched terminal path must not improve the path RMSE by truncation.
- Unknown/approximate/rank-deficient uncertainty must remain unavailable or explicitly partial; a model that always reports broad uncertainty should be distinguishable from one that is both calibrated and sharp.
- Include near-surface paths, short/late contacts, closely spaced sources, two-versus-three-source mismatch, no-source worlds, known-planet perturbations, measurement noise and withheld live captures. Freeze evaluation positions, worlds and thresholds before comparing variants; use independent worlds or captures, not ticks, for uncertainty around aggregate results.

## v2 estimator first-slice results (2026-11-26)

Observed with `field_scorecard` in a release build on the baseline databases (hashes unchanged from `estimator-baseline.md`), last completed-path checkpoint, vector RMSE; flat is the same scorer on an empty map. The v2 estimator models at most one planet, so multi-planet worlds are fitted by one compromise planet or none.

| Case | v2 explored / unexplored / whole | flat explored / unexplored / whole | v2 planets | cumulative fit ms (v1 baseline) |
| --- | --- | --- | ---: | --- |
| Synthetic 1, flat | 0 / 0 / 0 | 0 / 0 / 0 | 0 | 0.49 (0.60) |
| Synthetic 12, two flybys | 0.190 / 1.727 / 1.683 | 0.203 / 0.217 / 0.216 | 1 | 282 (77) |
| Synthetic 18, W7 | 0.630 / 4.198 / 4.032 | 0.612 / 0.622 / 0.621 | 1 | 3961 (60104) |
| Live 44, 8-path prefix | 0.853 / 0.564 / 0.572 | 0.854 / 0.566 / 0.574 | 1 | 670 (322) |
| Live 49, 8-path prefix | 5.678 / 0.652 / 1.539 | 0.595 / 0.613 / 0.612 | 1 | 1295 (2105) |

Reading: on the one-planet and flat fixtures v2 recovers the planet (centre error below 1 unit, radius below 0.1 on captures 2, 6, 10; no planet on capture 1). On the multi-planet cases it is worse than flat in unexplored area (12, 18) and in explored area on live 49, where the single inferred planet matched none of the 24 disclosed planets within 50 units. This is the one-planet model's misspecification, not yet fixed; it is the main open issue of this slice. Per-update wall time (10 cores) peaked at about 0.9 s on W7 (9 paths), 0.18 s on live 44, 0.41 s on live 49. These are single runs on an unisolated machine.

Held-out prediction (`targeting_report --fit-planets`, full-path coordinate RMSE of the next shot, using only earlier shots, v2 / flat): capture 2 0.000025 / 110.2; capture 6 0.000017 / 39.0; capture 10 0.051 / 9.93 (later shots 0.094/9.93, 0.278/32.3). These synthetic paths share the bot's simulator, so near-zero error says inference works under the assumed simulator, not that it works on the live engine.
