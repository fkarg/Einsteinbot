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

### E9c results (2026-11-27, branch `e9c`, builds on the E9 branch; harness `src/estimator/diagnose/polish.rs`, ignored test `diagnose_polish`)

Question: make a relative allowance part of the likelihood after the thinned search stage, and make the final exact polish drift-safe. Production unchanged. Stage 1 is the E9 cascade (cap 28000 evaluations, run once per world); every polish program then starts from that same map with a fresh counter (budget 12000 evaluations at 60 paths, 8000 at 16/8 paths, evaluations counted on full rows; thinned steps are weighted by node ratio in the "equiv" column). Truth only scores. Machine: jolly, taskset 24-29 (6 threads); results do not depend on thread count by construction (evaluation budgets) but were not re-run at another thread count. Program syntax is in the harness header. Programs: A `0gd` = E9 (exact refit, exact growth, exact deaths); B `0.005gd+0.001gd+0` = relative schedule on full rows (refit, growth and deaths with that level's evidence), then exact refit; C guard `...G3C1.5` = accept the exact refit only if its exact chi2 falls and (no start planet moves 3 units or the result has chi2/row <= 1.5), else keep the relative map; D `0.001gd+0` = one relative level then exact refit only (the "skip exact growth and deaths when K is complete" form); E `0.001gd+0gd` = D plus exact growth/deaths; F = schedule + guard + exact growth/deaths (`...C1.5+0gd`). Chi2 ratio is under the exact likelihood relative to the true map. Evaluations below are polish evaluations (full-row equivalents); add about 2.2-3k for stage 1 (equiv) or 23-28k raw evaluations.

Observed, complete stage-1 maps (synthetic dev 7001-7008, 60 paths, exact noise): every program ends 24/24, 0 spurious, ratio 0.994-0.996, position error < 0.01. Only the cost differs: mean polish evaluations A 6980 (range 860-12004), B 1550, D 820, E 8000, exact refit alone (`0G3`) 10-110 (but it keeps a stage-1 extra planet, 7001: K 25). The exact refit of a complete map costs 10-15 evaluations; A's cost is exact growth and deaths proving nothing is left.

Held-out synthetic 7101-7108, 60 paths (programs A, D, C):

| world | stage 1 rec2 | A rec2 / ratio / evals | D rec2 / evals | C rec2 / ratio / evals |
|---|---|---|---|---|
| 7101-7106 | 24 (7102: 23) | 24 / 0.994-0.998 / 6.1-12.0k | 24 / 0.45-0.8k | 24 / 0.995 / 0.95-1.6k |
| 7107 | 21 | 2 (15 spurious) / 1e11 / 12.0k | 3 / 4e9 / 12.5k | 22 (3 spurious, K 27) / 4.6e9 / 12.6k |
| 7108 | 22 | 18 / 4e6 / 11.3k | 19 / 4e6 / 3.3k | 22 / 5e7 / 6.3k |

Recall(2) >= 90%: A 6 of 8, C 8 of 8. The combined target (recall and ratio <= 1.2): A 6 of 8, C 6 of 8 (7107 and 7108 have ratio far above 1.2: the maps still miss 1-3 planets, and the exact likelihood punishes that). Mean polish evaluations: A 9380, D 2430, C 3280.

Incomplete stage-1 maps, dev, recall(2) of 24 (stage 1 / A / D / C / F):

| case | stage 1 | A | D | C | F |
|---|---|---|---|---|---|
| syn 16 paths 7001 | 24 | 24 | 24 | 24 | n/a |
| syn 16 paths 7002 | 15 | 15 (6 spurious) | 24 | 24 | 24 |
| syn 16 paths 7003 | 8 | 7 | 7 | 11 | 6 |
| syn 16 paths 7004 | 17 | 10 | 9 | 17 | 9 |
| syn 8 paths 7001-7004 | 10, 8, 1, 11 | 6, 9, 1, 6 | 9, 8, 3, 6 | 12, 18, 4, 11 | not run |
| syn noise 0.1, 60 paths 7001-7003 | 16, 22, 16 | 14, 5, 17 | 15, 24, 24 | 19, 24, 24 | not run |
| live 33 | 18 | 10 | 12 | 18 | 13 |
| live 39 | 21 | 22 | not run | 22 | 22 |
| live 40 | 18 | 21 (ratio 1.01, 4 spurious) | not run | 20 (ratio 460) | 20 (ratio 1.35) |
| live 41 | 17 | 10 | not run | 20 | 14 |
| live 45 | 12 | 10 | not run | 16 | 11 |
| live 48 | 22 | 24 (ratio 0.99) | not run | 24 (ratio 0.99) | not run |

Held-out live and few-path (recall(2); C = schedule + guard): live 34 (2 paths of data): stage 1 2, A/D/C 2. Live 35, 43: all 24, ratio 0.99; C costs 1.1-1.8k against A 1.4-4.9k. Live 37: stage 1 21, A 22 (ratio 0.99), D 18, C 21 (ratio 1e6), F 18. Live 42: stage 1 20, A 17, D 17, C 21 (ratio 2e7), F 17. Synthetic 16 paths 7101-7104: stage 1 19, 8, 16, 24; A 13, 6, 6, 24; D 15, 5, 6, 24; C 19, 9, 15, 24; F 15, 6, 7, not run.

Other observations:
- The exact polish of an incomplete map is the drift (observed: A ends below its stage-1 recall in 15 of the 42 cases tabulated here, 7107 from 21 to 2; C in 1 of 42, held-out 7103 with 16 paths, 16 to 15). Where the exact refit explains the data (live 48, chi2/row <= 1.5) the C override accepts it, so C costs 1.6k there against A 5.6k.
- The cause of most gains in B and C is the relative-level growth and deaths on full rows, not the guard alone: D alone recovers 7002/16 paths and noise-0.1 worlds completely (24/24), C adds more on the others. The exact growth after a guarded polish (F) is never better than C on recall in the cases tried (held-out 7107: 5 against 22; live 33, 41, 45, 37, 42 and the 16-path worlds lower; equal on live 39 and 40, where F has the better chi2 ratio on 40), so exact growth and deaths are what drifts, not the exact refit.
- A chi2-gated skip (`S`) is not usable: the stage-1 map has exact chi2/row of 400 to 10000 even when it has all 24 planets within 0.1 units (the relative-0.005 cascade positions are off by a small fraction of a unit but the exact likelihood is razor sharp), so an exact chi2 test cannot tell a complete map from an incomplete one. "Complete" is recognised by relative-level growth and deaths changing nothing (D/B cost 0.5-1.5k) and then the exact refit shrinking chi2 to about 1 (guard override).
- Evidence behaviour: with the relative allowance as the evidence (B, D) K ended at 24 on all complete synthetic worlds (the stage-1 K 25 of 7001 was removed by relative-level deaths; the exact refit alone kept it). On incomplete maps K overshoots with spurious planets (7107: K 27 with 3 spurious; live 33: K 26 with 6) because the relative term lets neighbours absorb a missing planet's pull; the exact evidence (A, E) stopped at K 23-25 as well but with more spurious (A on 7107: 15).
- Explored/unexplored field RMSE (acceleration units, 20-unit grid, explored = within 30 units of a path row) follows recall: complete maps 1e-6 / 1e-6, 7107 under C 2e-2 / 5e-1 against A 15 / 176. It is reported in every `RESULT` line.
- Few paths (8): C improves recall(2) over A in all four worlds (12/18/4/11 against 6/9/1/6) but is still far from the target; 16 paths: 2 of 4 worlds complete under C against 1 of 4 under A.
- Not done: relative sd above 0.005 on full rows as a permanent likelihood, because the cascade already runs those levels on thinned rows; a thinned relative polish (`t4000:` steps) was tried on live 33, 41, 45 and recovered about the same recall as full rows (21, 15, 19 against 20, 16, 18) at 70-80% of the cost, not enough to change the verdict.
- Data loss: the raw output files of the dev 60-path synthetic, 8/16-path, noise and first live runs were deleted by an rsync `--delete`; the numbers above for those were read from the run consoles before that, the held-out and later runs have their output on jolly (`/tmp/E9c-out`). All runs are reproducible from the commands below.

Verdict: adopt the relative-level polish as an opt-in, narrow form: relative-level growth and deaths on full rows (0.005, 0.001), then a guarded exact refit, and no exact growth/deaths on a map that the relative levels have not completed. It makes the E9 pipeline strictly safer (C's recall(2) is at least stage 1's in 41 of 42 cases tried, live included; A's in 27 of 42) and cuts the polish cost on complete maps from about 7-9k to 1.5-3k full-row evaluations. It does not meet the adoption target: held-out synthetic recall(2) >= 90% is 8 of 8 (A: 6 of 8) but the ratio <= 1.2 part stays at 6 of 8, live held-out 37 and 42 stay at ratio 1e6-1e7 (live truth is degenerate), and 8-path and noisy worlds stay far below target. Missing-planet completeness is the open problem, not the polish: the guard only stops losing planets, nothing here finds a stage-1 planet the cascade missed except through relative growth (which found several, e.g. 7002 with 16 paths, noise 0.1). Inconclusive on the exact-evidence question (A/E against B/D): equal results on complete maps, exact growth worse on incomplete ones.

Production search if all of this were adopted: (1) cascade growth on thinned rows (500/1000/2000 rows, relative sd 0.1/0.02/0.005), about 2.2-3k full-row-equivalent evaluations (23-28k raw); (2) refit the map on full rows at relative sd 0.005, grow and delete on that evidence, repeat at 0.001, about 0.8-2.2k on complete maps and up to the 8-12k allowance on incomplete ones; (3) exact refit (10-100 evaluations), accepted if the exact chi2 falls and (no start planet moves 3 units or chi2/row <= 1.5), else keep the relative map; (4) no exact growth or deaths. Expected cost: about 3.5-6k full-row equivalents on a complete map, up to about 15k on an incomplete one, against 9-15k for E9 as it was. Limits: dev and held-out worlds are the synthetic generator (24 planets, 60 or 16/8 long paths at speed 8) and ten live captures, one run per cell, thresholds (3 units, 1.5) not tuned beyond one revision (the C override was added after the dev live results; the held-out was run once with it, the F hybrid was tried on dev and then held-out cases once); the relative term uses the observed acceleration, which is noisy at noise 0.1.

Reproduction: `cargo test --release --lib diagnose_polish -- --ignored --nocapture --test-threads=1` with `PATHS`, `SEEDS`, `LIVE` (`-` for none), `NOISE`, `STAGE1`, `POLISH_BUDGET` and `POLISH` (comma-separated programs) as in the harness header, run in a directory with `einsteinbot.sqlite3` (the P4 capture subset in `/tmp/einsteinbot-p4`). Example for the held-out synthetic table: `PATHS=60 SEEDS=7101,7102,7103,7104,7105,7106,7107,7108 LIVE=- POLISH=0gd,0.001gd+0,0.005gd+0.001gd+0G3C1.5 POLISH_BUDGET=12000`.
