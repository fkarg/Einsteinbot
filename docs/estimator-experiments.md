# Estimator search experiments

Created 2026-11-26. Purpose: the v2 estimator's likelihood is validated, but greedy
births from flat never reach the truth basin at exact noise beyond ~8 planets
(P4 diagnosis, `docs/brainstorms/2026-11-13-newtonwars-bot.md`). This page lists the
ideas worth trying or comparing against the current search, how each is judged, and
where results go. It is the working list; it is not a decision record. Status values:
`todo`, `running`, `done: adopt`, `done: reject`, `done: inconclusive`.

Sources: user ideas, P4 worker recommendations, the independent peer (Round 51), and a
read of `Savo2610/newton-bot` (`src/mapper.js`; its README claims are unverified).
Oracle planet positions are for building, debugging and measuring only; live play never
has them, and they are never fit input.

## Common protocol (all experiments)

- **Data.** Development: synthetic seeds 7001-7008 (24 planets, the P4 generator,
  `diagnose.rs`) and live captures 33, 39, 40, 41, 45, 48. Held-out confirmation: fresh
  synthetic seeds 7101-7108 and live captures 34, 35, 37, 38, 42, 43, 46. Scorecard cases
  12/18/44/49 and capture 52 stay untouched until the session runs them once at the end.
- **Noise.** Exact (noise sd 0) for the search; also report 0.1 for the best variants.
- **Metrics per world.** (1) Recall: share of true planets with a fitted planet within 2
  units (and within 15 as a looser seed metric). (2) Spurious planets: fitted planets
  with no true planet within 15 units. (3) chi2/row of the final fit divided by the true
  map's chi2/row (target <= 1.2). (4) Final K. (5) Evaluations used (the work counter, not
  wall time). (6) Wall time as a secondary number, labelled with the machine and thread
  limit. Live truth is degenerate (several maps tie below the truth's chi2), so for live
  captures lead with the chi2 ratio and report recall second.
- **Reliability target for adoption.** Recall(2) >= 90% and chi2 ratio <= 1.2 on at least
  7 of 8 synthetic worlds, with no more spurious planets than the current search at equal
  or lower evaluations. Baseline: P4 greedy-from-flat gets about 0 planets within 2 units.
- **Determinism.** Use `Estimator.max_evaluations` for budgets. Results must not depend on
  thread count (state it if one does).
- **Heavy runs** go to `jolly` under a per-experiment directory in `/tmp` (32 cores,
  shared by concurrent experiments: limit each experiment to 6 cores with `taskset`, and
  do not use wall time for comparisons).
- **Grids are allowed only as seeding or acceleration heuristics.** The belief and the
  likelihood stay exact on planet parameters.

## Experiments

### E1. Free-space constraint (from newton-bot)
Every observed path point proves no planet centre lies within the radius (>= 20, so use
20) of it: the server ends a shot inside a planet. Use as (a) a seed filter (reject
candidate centres within r of any visited point) and (b) a soft hinge penalty in the fit.
Check the exact server rule in `../NewtonWars` (shot end test, radius, whether the
check is at substeps or ticks) before choosing the exclusion distance. Hypothesis: removes
a large class of wrong early births at nearly zero cost. Status: todo.

### E2. Nonnegative point-mass dictionary (newton-bot's step; peer option 2; session option B)
Rows are linear in each planet's mass given its centre. Over a dictionary of candidate
centres (heuristic grid or vote peaks), solve nonnegative least squares for all masses at
once with the exact windowed kernel, take the strongest peaks (several at once), refit
centres with LM, prune. One-by-one variant: orthogonal matching pursuit (pick the best
candidate, re-solve all masses linearly, repeat). Questions: required dictionary
resolution, near-singularity handling, bias toward many small planets (mass bound 800-6400
helps), cost of NNLS at 25k rows (row subsampling allowed, see E10), far-field kept in
scoring. Status: todo.

### E3. Strong-row one-by-one fitting (user + P4)
Select rows by strength (large observed bend, where one planet dominates) instead of by
distance to a seed (that variant, rows within 150-400 units of a wrong seed, already failed
in P4). Fit the dominant planet from strong rows only, then fix it and move to weaker rows.
Status: todo.

### E4. Seeds along the pull direction, triangulation and vote (user + P4)
A seed placed at the path point is off by the pass distance. Place seeds along the pull
direction at distance sqrt(m/|a|) for the mass range, intersect direction lines from
several rows (least squares), and vote over crossing points (a coarse accumulator is
fine). Measure the share of true planets with a candidate within 15 and within 3 units,
with and without the true background. First check how the current seeding places seeds
(`src/estimator/search.rs`); P4 measured only 36% within 15 units. Status: todo.

### E5. Repair moves (newton-bot)
(a) Periodic rebuild from empty, keep the better fit by a K-penalised score; (b)
delete-and-reseed of the worst local-residual planets after each round (newton-bot takes
the 4 worst by chi2 within 220 px); (c) swap/relocate moves. Test as add-ons to the
current greedy search first (does repair alone rescue it?), then on top of E2/E4.
Status: todo.

### E6. Contacts as candidate seeds (user)
A contact puts the centre within one radius (20-40) of the contact point. Use contacts as
candidate centres and radius information in seeding only; trajectory evidence still
decides, so a contact never creates a planet on its own (settled policy). Status: todo.

### E7. Non-overlap and prior rules (newton-bot)
If the server never places overlapping planets (verify in `../NewtonWars` placement code,
including the minimum gap), reject or merge overlapping hypotheses; use the
mass-radius coupling (m = r^3/10, r in 20-40) as a hard bound (already partly used).
Status: todo.

### E8. Cheaper optimization (P4)
A stall rule for creeping refits (little chi2 gain over N evaluations) in place of a looser
tolerance; the normal-equations LM (1.9x per evaluation but not equivalent, a different
optimizer); the radius-column shortcut (3 of 6 kernel applications per planet, unmeasured).
Status: todo.

### E9. Row thinning and relative sigma (newton-bot, user's coarse-to-fine)
newton-bot keeps at most 2 samples per 14 px cell (cap 2600), prefers strongly accelerated
samples, and uses sigma = f32 floor + 2% of |a|. Test (a) thinning to a few thousand rows
(cheap, near-exact) for the search stage with the full rows for the final fit; (b) a
relative-error term in the noise during the search only (the earlier annealed noise
homotopy failed, this form differs: it down-weights strong rows where unmodeled planets
dominate); the final fit and evidence stay exact. Status: done: adopt (as a search-stage
option, with caveats; see Results).

### Later (not in the current round)
- Targeting from newton-bot: p_hit averaged over covariance-sampled models; a separate
  targeting thread (Q1).
- Kalman-style local update; freezing settled planets (user).
- Per-planet fast multipole: rejected for estimation by P4 (no useful cutoff radius).
- Random restarts and tempered SMC / population search over whole configurations, only if
  the experiments above fail.

## Results

Append one subsection per experiment: date, branch or worktree, tables on the metrics
above, verdict, and a line on what it changes in the production search.

### E9 results (2026-11-26, branch `worktree-wf_3d9f39da-929-5`, W5-thinning-sigma)

Harness: `src/estimator/diagnose/thinning.rs` (ignored release test `diagnose_thinning_search`),
plus additive hooks in `window.rs` (`PathData::new_relative`, `thinned`) and `fit.rs`
(`Fit::field` visibility, `node_range`). Production search is unchanged. Truth is used only to
score. Machine: jolly, 6 cores (taskset 24-29); budgets are evaluation counts; "equiv" weights
search-stage evaluations by their node count relative to the full rows. Paths: 60 per synthetic
world (about 12-16k rows; the earlier 8-path runs are in the table's last rows).

Design that works (the "cascade"): grow K greedily on thinned rows with a relative noise term,
then continue growing on more rows with a smaller relative term, then refit once on the exact
rows and let the exact evidence and deaths finish. Stage rows: at most 2 per 14-unit cell,
strongest observed acceleration first, cap 500 / 1000 / 2000, relative sd 0.1 / 0.02 / 0.005 of
the observed |a|. Spec: `thinrel:14:2:500:strong:0.1+thinrel:14:2:1000:strong:0.02+thinrel:14:2:2000:strong:0.005`.

Growth stage alone (map as found, no exact refit), 60000-evaluation cap, 60 paths:

| search objective | worlds | recall(2) | recall(15) | spurious | K | evals (equiv) |
|---|---|---|---|---|---|---|
| exact greedy (baseline) | syn 7001-7005, g7 live | about 0% | 2-26% | 7-13 | 14-18 | 60000 |
| all rows, marginal variance only (no thinning) | syn 7001-7003 | 1-4 of 24 | 6-17 | 6-14 | 20-23 | 60000 |
| thin 500 only | syn dev 8 | 92% | 95% | 1.4 | 24.2 | 35.6k (3.0k) |
| thin 500 only | live dev 6 | 68% | 80% | 5.0 | 24.2 | 50.6k (6.5k) |
| relative 0.1 only, all rows | syn dev 8 | 79% | 91% | 3.2 | 25.1 | 50.1k (50.1k) |
| thin 500 + relative 0.1 | syn dev 8 | 88% | 94% | 0.1 | 22.8 | 24.5k (2.0k) |
| cascade (500/0.1, 1000/0.02) | syn dev 8 | 98% | 99% | 0.2 | 24.0 | 25.6k (2.1k) |
| cascade (3 stages) | syn dev 8 | 100% | 100% | 0.0 | 24.1 | 25.8k (2.2k) |
| cascade (3 stages) | live dev 6 | 82% | 88% | 2.8 | 24.2 | 33.9k (4.5k) |
| cascade (3 stages) | syn held-out 8 | 97% | 98% | 0.4 | 24.0 | 25.2k (2.2k) |

Full pipeline (cascade, then exact refit, exact growth and deaths; budget 40000, stage 1 at most
70%), 60 paths, target recall(2) >= 90% and chi2 ratio <= 1.2:

| worlds | meet target | notes |
|---|---|---|
| synthetic dev 7001-7008 | 8 of 8 | K 24, 0 spurious, chi2 ratio 0.993-0.996, 28-40k evaluations |
| synthetic held-out 7101-7108 | 6 of 8 | 7107: stage 1 had 21 of 24, the exact refit then drifted (recall 2, 15 spurious); 7108: 18 of 24, ratio 4e6 |
| live dev 33,39,40,41,45,48 | 1 of 6 on both criteria (48); chi2 ratio <= 1.2 only for 48 (40: 1.41) | 33/41/45 stage 1 incomplete (17-18 of 24), exact refit drifts; truth is degenerate on live |
| live held-out 35,37,42,43 | 35, 37 (22/24), 43 pass; 42 fails (17 of 24, ratio 2.6e5) | 34 has 2 paths, 38 and 46 have none in the first epoch: no data |
| exact greedy, same budget | 0 of 6 synthetic | recall(2) 0-1 of 24, 7-13 spurious |

Other measurements:
- Noise sd 0.1 (2 synthetic worlds only, partial): cascade ends at K 24, recall(2) 14 of 24, 3
  spurious, chi2 ratio 5e6; exact greedy recall 0. The cascade's relative terms were tuned for
  exact noise; not adopted for noisy data without more work.
- 8 paths (about 2000 rows, thinning barely thins): cascade growth-only recall(2) 58% syn /
  38% live (exact greedy 3% / 11%). Better, but not a solution for few paths.
- Rejected variants: re-thinning each round by the whitened residual of the current map (K 8-9
  with 6-9 spurious: worse); freezing the found planets and continuing to add (K 32 with 9
  spurious on 7001); an annealed full-row refit of the stage-1 map (recall dropped to 0-2);
  adding full-row relative stages (0.002, 0.0005) after the cascade (no gain, 5-10x the cost);
  the spread (random per cell) instead of strongest-first preference (recall 57-59%).
- Stage-1 budget: earlier 15000-evaluation runs were budget-capped, not converged, and
  understated the method; growth converges at about 20-28k evaluations.
- The exact refit of an incomplete stage-1 map (any 3+ planets missing) is the weak point: the
  planets drift off at exact noise (e.g. 7003 single stage: recall 16 -> 0 within 3000 evaluations).

Verdict: adopt as an opt-in search stage. The effect comes from the sparse strongest-row
selection plus the relative term; relative alone and thinning alone each help (79% / 92%) but
leave more spurious planets than the combination, and decorrelated all-row marginal noise does
not help (so it is the row selection, not the noise form). Difference from the failed annealed
noise: this changes the row set and growth continues on each objective, whereas annealing refit a
fixed map. Evidence limits: synthetic generator only (24 planets, 60 long paths); live results
are mixed; 8-path and noisy data are not solved; the cascade parameters were tuned on the dev
worlds (7001-7008), though held-out recall was 97% growth-only and 6 of 8 full.
