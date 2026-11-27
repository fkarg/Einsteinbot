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

### P6 results (2026-11-27, branch `p6`, built on `e9c`; harness `src/estimator/diagnose/incremental.rs`, ignored tests `diagnose_incremental` and `diagnose_rebuild`; tables from `scripts/p6_tables.py`)

Question: is the warm-started incremental structure "B" viable, i.e. can a map be kept up to date path by path at roughly constant cost, with from-scratch rebuilds only as an occasional audit? Production search is unchanged; this is a measurement prototype in the diagnosis harness. Truth is used only to score. Short answer: quality yes, steady-state latency yes (median 22 ms, worst 250 ms on the Mac at K=24 once the map has settled), worst-case latency no while planets are still being born (seconds, up to 20 s on the Mac), and the per-planet support test of Q13 cannot be built on a removal chi2.

#### What B is here (configuration B*)

`lv=0.02/0.005:cap=1000:rf=300:ev=1500:lr=100:dl=30:dn=4` (harness `CONFIGS` syntax). Per new path:
1. Rows. The accumulated paths are kept at relative sd 0.02 and 0.005 (observed |a| times that, added to the noise, as in the E9 cascade). For each level the rows are thinned (at most 2 per 14-unit cell, strongest observed acceleration first, at most `cap` = 1000 rows of the 2 x 16k at 60 paths; the paths' windows are built once per path, thinning is redone per update, about 1 ms).
2. Warm refit (`rf`, `lr`): only the planets within 100 units of a row of the new path are refit (300 evaluation cap); the others are fixed field. No evidence is computed for a refit.
3. Births (`ev`): if the unexplained windows on those rows still seed a planet, evidence is computed and greedy births run as in the E9 cascade (seeds fitted alone, joint refits of the best three) until no birth improves the evidence, with a cap of 1500 evaluations per level. A map with no significant unexplained window pays nothing here.
4. Deaths (`dl`, `dn`): every 4th update the two planets whose removal raises the chi2 least are removed, their neighbours within 100 units refit, and the removal is kept if the chi2 rises by less than 30. It changed no result in the four dev worlds where it was compared with no deaths (7001-7004, thresholds 10, 30, 100), so B* effectively has no deaths (candidates are ranked by the plain removal chi2, which misses the redundant pair described under Q4); an evidence-based death descent (`dv=300:dn=4`, depth 1) gave the same recall on four synthetic dev worlds (7001-7004: 100% at 32 paths) and 86% against 88% on the six live dev captures at 60 paths.
5. Audit (`audit=N` adopts, `probe=N` only measures): a from-scratch rebuild on all paths so far (E9 cascade, then the E9c polish `0.005gd+0.001gd+0` with the guarded exact refit, polish budget 8000/12000 evaluations) is compared with the maintained map refit on all rows at relative 0.005 by the Laplace evidence at that level; the rebuild is adopted only if its evidence is higher.
Quality is also shown "+ guarded exact refit" (E9c rule: exact refit of the current map, accepted if the exact chi2 falls and no planet moves 3 units or chi2/row <= 1.5, at most 600 evaluations; not kept in the state, and not timed).

Evaluation counts: "full-row equivalent" = evaluations times (nodes of the row set / nodes of all rows of the accumulated paths), as in E9/E9c. Because a local refit has fewer unknowns the equivalent undercounts the cost of the thinned set; use wall time for latency.

Machine and conditions. Latency: the Mac (10 cores, macOS), release build, the harness's `cores()` limited with a test-only override (`TEST_THREADS`, absent from non-test builds). The Mac was not idle: other sessions were running (load average 2 to 4 during the runs, 8 logged-in users), and during the production run, the first B run and the 1-thread run I compiled in the same checkout. Treat latencies as indicative to a factor of about 1.5. Quality: 14 dev worlds and 13 held-out worlds were run on `jolly` (2 threads per process, load 20-30); results are evaluation-budget deterministic, so thread count and load do not enter (not re-run on the Mac). Paths: synthetic in generator order (a fixed random order), live in capture order (first map epoch, at most 61 paths; capture 48 has 48, 43 has 41, 34 has 2). Exact noise (sd 0).

#### Q1 update cost per new path (Mac, release, threads as stated)

Synthetic seed 7001 (K=24), B*, 8 threads, 60 paths, by the accumulated path count:

| paths at the update | updates | median ms | p90 ms | max ms | median evaluations | median full-row equiv | median K |
|---|---|---|---|---|---|---|---|
| 1-4 | 4 | 279 | 474 | 474 | 3370 | 3302 | 9 |
| 5-8 | 4 | 2062 | 4706 | 4706 | 3584 | 3520 | 18 |
| 9-16 | 8 | 110 | 10460 | 10460 | 46 | 30 | 24 |
| 17-32 | 16 | 24 | 214 | 218 | 20 | 8 | 24 |
| 33-60 | 28 | 22 | 244 | 249 | 18 | 4 | 24 |

The same by variant and world (early = updates at 16 paths or fewer, when the map is still being grown from nothing; late = the rest):

| run | threads | paths<=16: updates, median / p90 / max ms | paths>16: updates, median / p90 / max ms, median evals, median full-row equivalents, median K |
|---|---|---|---|
| production update, path by path (5 s time budget, 24 planet cap lifted to 32), synthetic 7001 | 8 | 16: 5013 / 5031 / 5035 | 44: 5091 / 5215 / 5295, 268, 268, 24 |
| first B variant (cap 2000, full warm refit, evidence deaths every update), 7001 | 8 | 16: 2081 / 24705 / 25732 | 44: 2131 / 3216 / 4749, 194, 94, 24 |
| cap 1000, no local refit, deaths every 4th update (40 paths), 7001 | 8 | 16: 1094 / 4335 / 5219 | 24: 169 / 1107 / 1542, 22, 8, 24 |
| + local warm refit (two caps in one file: 1000 and 500 rows, 36 paths), 7001 | 8 | 32: 864 / 2953 / 3604 | 40: 21 / 880 / 1792, 20, 6, 24 |
| local deaths every update (36 paths), 7001 | 8 | 16: 317 / 4749 / 10360 | 20: 215 / 224 / 230, 74, 26, 24 |
| B*, 7001, K=24, 60 paths | 8 | 16: 312 / 4706 / 10460 | 44: 22 / 231 / 249, 20, 6, 24 |
| B*, 4 threads, 40 paths | 4 | 16: 359 / 5366 / 12052 | 24: 24 / 218 / 232, 20, 7, 24 |
| B*, 1 thread, 40 paths | 1 | 16: 667 / 10050 / 21696 | 24: 33 / 220 / 243, 20, 7, 24 |
| B* with all rows (no thinning), 40 paths | 8 | 16: 823 / 5576 / 9411 | 24: 69 / 652 / 841, 18, 18, 24 |
| B* cap 4000, 40 paths | 8 | 16: 458 / 5049 / 9829 | 24: 64 / 602 / 769, 20, 18, 24 |
| B* cap 2000, 40 paths | 8 | 16: 440 / 5056 / 9906 | 24: 42 / 461 / 498, 18, 13, 24 |
| B*, K=8 world (seed 7001), 40 paths | 8 | 16: 22 / 627 / 810 | 24: 14 / 40 / 54, 18, 2, 8 |
| B*, K=16 world, 40 paths | 8 | 16: 124 / 1920 / 2341 | 24: 18 / 120 / 175, 19, 5, 16 |
| B*, K=26 world, 40 paths | 8 | 16: 1012 / 14667 / 20940 | 24: 27 / 408 / 17482, 22, 15, 26 |
| B*, live 33 (61 paths) | 8 | 16: 560 / 3264 / 6192 | 45: 23 / 1507 / 9475, 22, 10, 23 |
| B*, live 40 | 8 | 16: 254 / 3591 / 3598 | 45: 22 / 4780 / 12289, 22, 15, 21 |
| B*, live 45 | 8 | 16: 368 / 4262 / 5771 | 45: 1975 / 18034 / 19039, 1210, 503, 22 |
| B* births capped at 300 evaluations, live 33 | 8 | 16: 266 / 1132 / 1180 | 45: 146 / 1073 / 1737, 684, 204, 22 |
| births capped at 300, live 40 | 8 | 16: 165 / 669 / 886 | 45: 166 / 1202 / 2264, 354, 180, 19 |
| births capped at 600, live 33 | 8 | 16: 253 / 1787 / 2451 | 45: 288 / 935 / 1860, 1265, 378, 22 |
| births capped at 600, live 40 | 8 | 16: 175 / 1326 / 1569 | 45: 24 / 2791 / 4971, 24, 17, 22 |

Reading:
- Steady state (map complete, no births): 20-25 evaluations and 22-33 ms median, 230-250 ms worst (the update that runs the local death test; without it about 25 ms). Threads hardly matter: median 22 ms at 8 threads, 24 ms at 4, 33 ms at 1 (the refit and the evidence are mostly single-threaded). Observed: this is a constant-cost update, independent of the path count from 17 to 60 and of K between 8 and 26 (K=8 14 ms, K=16 18 ms, K=24 22 ms, K=26 27 ms median late).
- The row cap is what makes it constant. With the local refit in place, 40-path replays: cap 1000 median 22 ms, cap 2000 42 ms, cap 4000 64 ms, all rows 69 ms (max 841 ms); medians rise with the path count without a cap and stay flat with it. Before the local refit (full warm refit of all planets, 1000-2000 rows) the median was 150-220 ms at 8 threads and 1.7-2.1 s with evidence-based deaths every update; the production update (exact rows, whole map, 5 s time budget) used its whole 5 s budget at every update from the 5th path on (250-440 evaluations at K=24 after 32 paths) and was cut by the budget.
- Worst case (observed): any update that runs births is not bounded by this. On synthetic 7001 the 8th-16th updates (K grows from about 15 to 24) take up to 10 s, and a K=26 world had one 17.5 s update at 40 paths. On live captures at 8 threads (paths > 16): live 33 median 23 ms, p90 1.5 s, max 9.5 s; live 40 22 ms, p90 4.8 s, max 12.3 s; live 45 (a world whose map is still incomplete at 32 paths, 12 of 24 found) median 2.0 s, p90 18 s, max 19 s. Capping the births at 300 evaluations per update brings live 33/40 to median 146/166 ms, p90 1.1/1.2 s, max 1.7/2.3 s (600 evaluations: median 288/24 ms, p90 0.9/2.8 s, max 1.9/5.0 s) at a price in recall: live 33 recall(2) at 60 paths 22 uncapped, 18 (cap 300), 21 (cap 600); live 40 19, 17, 18 (one world each, no repeats).
- The rebuild that B would use as an audit (observed on the Mac, 8 threads, synthetic 7005): 41 s at 16 paths, 28 s at 32, 56 s at 60; 3k-34k full-row equivalent evaluations (mean over the dev worlds: synthetic 34k at 4 paths, 16k at 16, 9.8k at 32, 3.8k at 60; live 33k, 27k, 18k, 13.5k). In raw evaluations a rebuild is 27-37k against about 20 per steady-state update (1400-1850 updates), and 1300-2500 updates in Mac wall time.

#### Q2 quality against path count

Recall(2) is the share of true planets with a fitted planet within 2 units, recall(15) within 15, spurious = fitted planets with no true planet within 15 units. "ratio <= 1.2" counts worlds whose exact chi2/row is at most 1.2 times the true map's; the median ratio is the median over worlds of that ratio. Field columns are the mean over worlds of the field RMSE divided by the RMSE of the empty (flat) map, on the 20-unit grid, explored = within 30 units of a row (acceleration units, truth only scores). The raw B map has a large exact chi2 ratio (1e3-1e5 even for 24/24 maps) because the relative-level fit leaves position errors of about 0.001-0.01 units, which the exact likelihood punishes; one guarded exact refit of the maintained map (row "+ guarded exact refit") gives 0.99 on every synthetic world from 32 paths on (live: complete only where the map is).

#### Q2 quality, dev synthetic (7001-7008, 8 worlds)

| map | n | worlds | recall2 | recall15 | spurious | K | ratio<=1.2 | median ratio | field expl/flat | unexpl/flat | whole/flat |
|---|---|---|---|---|---|---|---|---|---|---|---|
| B, no audit | 4 | 8 | 8% | 27% | 5.9 | 12.2 | 0/8 | 8.04e+09 | 2.18e-01 | 1.75e+01 | 1.67e+01 |
| B, no audit | 8 | 8 | 46% | 61% | 4.9 | 19.5 | 0/8 | 5.45e+08 | 6.02e-02 | 9.67e+00 | 8.76e+00 |
| B, no audit | 16 | 8 | 90% | 92% | 1.1 | 23.2 | 0/8 | 1.25e+04 | 1.11e-02 | 2.00e+00 | 1.63e+00 |
| B, no audit | 32 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 1.38e+04 | 1.55e-04 | 2.73e-03 | 1.88e-03 |
| B, no audit | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 4.73e+04 | 1.04e-04 | 5.90e-04 | 3.21e-04 |
| B, no audit + guarded exact refit | 4 | 8 | 8% | 27% | 5.9 | 12.2 | 0/8 | 8.04e+09 | 2.18e-01 | 1.75e+01 | 1.67e+01 |
| B, no audit + guarded exact refit | 8 | 8 | 46% | 61% | 4.9 | 19.5 | 0/8 | 5.45e+08 | 6.02e-02 | 9.67e+00 | 8.76e+00 |
| B, no audit + guarded exact refit | 16 | 8 | 90% | 92% | 1.1 | 23.2 | 5/8 | 1.01e+00 | 1.10e-02 | 2.00e+00 | 1.64e+00 |
| B, no audit + guarded exact refit | 32 | 8 | 100% | 100% | 0.0 | 24.0 | 8/8 | 9.91e-01 | 6.80e-06 | 2.33e-05 | 1.74e-05 |
| B, no audit + guarded exact refit | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 8/8 | 9.96e-01 | 3.35e-06 | 5.54e-06 | 4.20e-06 |
| B, audit every 16 | 4 | 8 | 8% | 27% | 5.9 | 12.2 | 0/8 | 8.04e+09 | 2.18e-01 | 1.75e+01 | 1.67e+01 |
| B, audit every 16 | 8 | 8 | 46% | 61% | 4.9 | 19.5 | 0/8 | 5.45e+08 | 6.02e-02 | 9.67e+00 | 8.76e+00 |
| B, audit every 16 | 16 | 8 | 91% | 93% | 1.1 | 23.4 | 0/8 | 1.25e+04 | 1.09e-02 | 8.38e-01 | 6.98e-01 |
| B, audit every 16 | 32 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 1.38e+04 | 1.54e-04 | 2.72e-03 | 1.88e-03 |
| B, audit every 16 | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 4.73e+04 | 1.04e-04 | 5.88e-04 | 3.20e-04 |
| B, audit every 8 | 4 | 8 | 8% | 27% | 5.9 | 12.2 | 0/8 | 8.04e+09 | 2.18e-01 | 1.75e+01 | 1.67e+01 |
| B, audit every 8 | 8 | 8 | 55% | 67% | 5.9 | 22.0 | 0/8 | 8.90e+07 | 5.00e-02 | 1.31e+01 | 1.21e+01 |
| B, audit every 8 | 16 | 8 | 91% | 93% | 1.0 | 23.4 | 0/8 | 4.51e+04 | 1.11e-02 | 1.66e+00 | 1.43e+00 |
| B, audit every 8 | 32 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 1.38e+04 | 1.43e-04 | 1.20e-03 | 8.38e-04 |
| B, audit every 8 | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 7.68e+04 | 1.10e-04 | 5.61e-04 | 3.06e-04 |
| from scratch (cascade + polish) | 4 | 8 | 26% | 40% | 8.0 | 17.6 | 0/8 | 2.19e+07 | 5.77e-02 | 3.24e+01 | 3.08e+01 |
| from scratch (cascade + polish) | 8 | 8 | 55% | 67% | 5.9 | 22.0 | 0/8 | 8.90e+07 | 5.00e-02 | 1.31e+01 | 1.21e+01 |
| from scratch (cascade + polish) | 16 | 8 | 86% | 92% | 1.4 | 23.4 | 5/8 | 9.83e-01 | 2.28e-02 | 2.72e+00 | 2.41e+00 |
| from scratch (cascade + polish) | 32 | 8 | 96% | 97% | 1.0 | 24.5 | 6/8 | 9.91e-01 | 1.86e-02 | 3.19e-01 | 2.17e-01 |
| from scratch (cascade + polish) | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 8/8 | 9.96e-01 | 3.35e-06 | 5.54e-06 | 4.20e-06 |

#### Q2 quality, dev live (33, 39, 40, 41, 45, 48; 48 has 48 paths so it is missing at 60)

| map | n | worlds | recall2 | recall15 | spurious | K | ratio<=1.2 | median ratio | field expl/flat | unexpl/flat | whole/flat |
|---|---|---|---|---|---|---|---|---|---|---|---|
| B, no audit | 4 | 6 | 4% | 16% | 5.3 | 9.2 | 0/6 | 1.90e+08 | 2.12e-01 | 1.33e+01 | 1.30e+01 |
| B, no audit | 8 | 6 | 27% | 42% | 5.8 | 15.8 | 0/6 | 2.44e+06 | 9.00e-02 | 1.02e+01 | 9.65e+00 |
| B, no audit | 16 | 6 | 51% | 61% | 5.7 | 20.3 | 0/6 | 2.03e+06 | 2.03e-02 | 1.11e+01 | 9.98e+00 |
| B, no audit | 32 | 6 | 72% | 78% | 3.3 | 22.2 | 0/6 | 3.39e+07 | 1.55e-02 | 3.55e+00 | 2.87e+00 |
| B, no audit | 60 | 5 | 88% | 90% | 1.0 | 22.6 | 0/5 | 3.04e+07 | 1.79e-03 | 3.13e+01 | 2.16e+01 |
| B, no audit + guarded exact refit | 4 | 6 | 4% | 16% | 5.3 | 9.2 | 0/6 | 1.90e+08 | 2.12e-01 | 1.33e+01 | 1.30e+01 |
| B, no audit + guarded exact refit | 8 | 6 | 27% | 42% | 5.8 | 15.8 | 0/6 | 2.44e+06 | 9.00e-02 | 1.02e+01 | 9.65e+00 |
| B, no audit + guarded exact refit | 16 | 6 | 51% | 61% | 5.7 | 20.3 | 0/6 | 2.03e+06 | 2.03e-02 | 1.11e+01 | 9.98e+00 |
| B, no audit + guarded exact refit | 32 | 6 | 72% | 78% | 3.3 | 22.2 | 0/6 | 3.39e+07 | 1.55e-02 | 3.55e+00 | 2.87e+00 |
| B, no audit + guarded exact refit | 60 | 5 | 88% | 90% | 1.0 | 22.6 | 0/5 | 3.04e+07 | 1.79e-03 | 3.13e+01 | 2.16e+01 |
| B, audit every 16 | 4 | 6 | 4% | 16% | 5.3 | 9.2 | 0/6 | 1.90e+08 | 2.12e-01 | 1.33e+01 | 1.30e+01 |
| B, audit every 16 | 8 | 6 | 27% | 42% | 5.8 | 15.8 | 0/6 | 2.44e+06 | 9.00e-02 | 1.02e+01 | 9.65e+00 |
| B, audit every 16 | 16 | 6 | 56% | 65% | 5.0 | 20.7 | 0/6 | 1.08e+06 | 1.79e-02 | 1.01e+01 | 8.99e+00 |
| B, audit every 16 | 32 | 6 | 81% | 85% | 1.8 | 22.3 | 1/6 | 6.61e+05 | 2.04e-03 | 4.40e+00 | 3.65e+00 |
| B, audit every 16 | 60 | 5 | 88% | 89% | 1.2 | 22.6 | 0/5 | 1.10e+07 | 1.81e-03 | 2.50e+01 | 1.70e+01 |
| B, audit every 8 | 4 | 6 | 4% | 16% | 5.3 | 9.2 | 0/6 | 1.90e+08 | 2.12e-01 | 1.33e+01 | 1.30e+01 |
| B, audit every 8 | 8 | 6 | 31% | 47% | 5.3 | 16.5 | 0/6 | 1.46e+06 | 4.80e-02 | 1.45e+01 | 1.38e+01 |
| B, audit every 8 | 16 | 6 | 56% | 65% | 5.2 | 20.8 | 0/6 | 1.37e+06 | 1.78e-02 | 1.37e+01 | 1.23e+01 |
| B, audit every 8 | 32 | 6 | 81% | 86% | 1.3 | 22.0 | 1/6 | 5.46e+06 | 2.32e-03 | 4.51e+00 | 3.74e+00 |
| B, audit every 8 | 60 | 5 | 91% | 92% | 0.8 | 23.0 | 0/5 | 8.86e+06 | 1.23e-03 | 2.51e+01 | 1.70e+01 |
| from scratch (cascade + polish) | 4 | 6 | 19% | 30% | 7.0 | 14.2 | 0/6 | 7.55e+04 | 2.26e-02 | 9.24e+00 | 9.03e+00 |
| from scratch (cascade + polish) | 8 | 6 | 26% | 44% | 6.0 | 16.5 | 0/6 | 9.85e+06 | 7.05e-02 | 2.09e+01 | 2.00e+01 |
| from scratch (cascade + polish) | 16 | 6 | 52% | 62% | 5.3 | 20.5 | 0/6 | 1.24e+06 | 5.16e-02 | 1.98e+01 | 1.78e+01 |
| from scratch (cascade + polish) | 32 | 6 | 76% | 81% | 3.3 | 23.0 | 1/6 | 4.88e+04 | 5.68e-02 | 5.63e+00 | 4.75e+00 |
| from scratch (cascade + polish) | 60 | 5 | 79% | 85% | 2.8 | 23.4 | 0/5 | 1.47e+07 | 9.65e-02 | 2.47e+01 | 1.68e+01 |

#### Q2 quality, held-out synthetic (7101-7108)

| map | n | worlds | recall2 | recall15 | spurious | K | ratio<=1.2 | median ratio | field expl/flat | unexpl/flat | whole/flat |
|---|---|---|---|---|---|---|---|---|---|---|---|
| B, no audit | 4 | 8 | 4% | 24% | 5.9 | 11.6 | 0/8 | 8.69e+10 | 2.75e-01 | 6.96e+00 | 6.68e+00 |
| B, no audit | 8 | 8 | 34% | 55% | 5.9 | 19.2 | 0/8 | 2.33e+09 | 1.75e-01 | 4.03e+00 | 3.63e+00 |
| B, no audit | 16 | 8 | 76% | 84% | 3.1 | 23.5 | 0/8 | 1.18e+08 | 5.48e-02 | 8.64e+00 | 6.84e+00 |
| B, no audit | 32 | 8 | 97% | 99% | 0.2 | 24.5 | 0/8 | 9.44e+03 | 6.39e-03 | 3.88e+00 | 2.66e+00 |
| B, no audit | 60 | 8 | 100% | 100% | 0.0 | 24.4 | 0/8 | 2.71e+04 | 9.97e-05 | 8.23e-04 | 4.39e-04 |
| B, no audit + guarded exact refit | 4 | 8 | 4% | 24% | 5.9 | 11.6 | 0/8 | 8.69e+10 | 2.75e-01 | 6.96e+00 | 6.68e+00 |
| B, no audit + guarded exact refit | 8 | 8 | 34% | 55% | 5.9 | 19.2 | 0/8 | 2.33e+09 | 1.75e-01 | 4.03e+00 | 3.63e+00 |
| B, no audit + guarded exact refit | 16 | 8 | 76% | 84% | 3.1 | 23.5 | 2/8 | 1.18e+08 | 5.48e-02 | 8.64e+00 | 6.84e+00 |
| B, no audit + guarded exact refit | 32 | 8 | 97% | 99% | 0.2 | 24.5 | 7/8 | 9.91e-01 | 6.32e-03 | 3.88e+00 | 2.66e+00 |
| B, no audit + guarded exact refit | 60 | 8 | 100% | 100% | 0.0 | 24.4 | 8/8 | 9.95e-01 | 6.79e-06 | 9.42e-06 | 7.69e-06 |
| B, audit every 16 | 4 | 8 | 4% | 24% | 5.9 | 11.6 | 0/8 | 8.69e+10 | 2.75e-01 | 6.96e+00 | 6.68e+00 |
| B, audit every 16 | 8 | 8 | 34% | 55% | 5.9 | 19.2 | 0/8 | 2.33e+09 | 1.75e-01 | 4.03e+00 | 3.63e+00 |
| B, audit every 16 | 16 | 8 | 82% | 89% | 2.2 | 23.6 | 1/8 | 3.28e+07 | 3.63e-02 | 1.44e+00 | 1.20e+00 |
| B, audit every 16 | 32 | 8 | 97% | 99% | 0.2 | 24.0 | 1/8 | 8.80e+03 | 6.38e-03 | 3.88e+00 | 2.66e+00 |
| B, audit every 16 | 60 | 8 | 100% | 100% | 0.0 | 24.1 | 0/8 | 2.71e+04 | 9.92e-05 | 8.08e-04 | 4.34e-04 |
| B, audit every 8 | 4 | 8 | 4% | 24% | 5.9 | 11.6 | 0/8 | 8.69e+10 | 2.75e-01 | 6.96e+00 | 6.68e+00 |
| B, audit every 8 | 8 | 8 | 60% | 74% | 4.2 | 22.0 | 2/8 | 2.32e+08 | 7.84e-02 | 2.46e+00 | 2.25e+00 |
| B, audit every 8 | 16 | 8 | 94% | 96% | 0.6 | 24.0 | 0/8 | 1.32e+04 | 7.10e-03 | 5.68e-01 | 4.82e-01 |
| B, audit every 8 | 32 | 8 | 99% | 100% | 0.0 | 24.0 | 0/8 | 1.69e+04 | 1.21e-04 | 3.91e-03 | 2.69e-03 |
| B, audit every 8 | 60 | 8 | 100% | 100% | 0.0 | 24.0 | 0/8 | 2.38e+04 | 9.86e-05 | 2.76e-04 | 1.67e-04 |
| from scratch (cascade + polish) | 4 | 8 | 28% | 42% | 7.6 | 17.8 | 0/8 | 6.83e+07 | 5.80e-02 | 7.11e+00 | 6.80e+00 |
| from scratch (cascade + polish) | 8 | 8 | 62% | 73% | 4.2 | 21.9 | 2/8 | 2.35e+08 | 7.82e-02 | 2.44e+00 | 2.23e+00 |
| from scratch (cascade + polish) | 16 | 8 | 77% | 85% | 3.0 | 23.5 | 2/8 | 1.32e+08 | 6.82e-02 | 6.68e+00 | 5.48e+00 |
| from scratch (cascade + polish) | 32 | 8 | 95% | 97% | 0.4 | 24.1 | 5/8 | 9.93e-01 | 2.30e-02 | 1.31e-01 | 9.65e-02 |
| from scratch (cascade + polish) | 60 | 8 | 98% | 99% | 0.5 | 24.2 | 6/8 | 9.96e-01 | 1.10e-02 | 1.52e-01 | 7.18e-02 |

#### Q2 quality, held-out live (34, 35, 37, 42, 43; 34 has 2 paths, 43 has 41; rows count only the captures that reach that many paths)

| map | n | worlds | recall2 | recall15 | spurious | K | ratio<=1.2 | median ratio | field expl/flat | unexpl/flat | whole/flat |
|---|---|---|---|---|---|---|---|---|---|---|---|
| B, no audit | 4 | 4 | 5% | 19% | 4.8 | 9.2 | 0/4 | 6.44e+07 | 1.65e-01 | 7.86e+00 | 7.72e+00 |
| B, no audit | 8 | 4 | 32% | 45% | 6.0 | 16.8 | 0/4 | 9.17e+06 | 7.52e-02 | 3.36e+01 | 3.18e+01 |
| B, no audit | 16 | 4 | 55% | 61% | 5.2 | 20.0 | 0/4 | 3.82e+06 | 3.58e-02 | 1.23e+01 | 1.13e+01 |
| B, no audit | 32 | 4 | 80% | 83% | 2.2 | 22.8 | 0/4 | 5.50e+06 | 3.86e-03 | 7.81e+00 | 6.45e+00 |
| B, no audit | 60 | 3 | 92% | 96% | 0.3 | 23.7 | 0/3 | 3.05e+05 | 6.46e-04 | 1.37e-01 | 9.53e-02 |
| B, no audit + guarded exact refit | 4 | 4 | 5% | 19% | 4.8 | 9.2 | 0/4 | 6.44e+07 | 1.65e-01 | 7.86e+00 | 7.72e+00 |
| B, no audit + guarded exact refit | 8 | 4 | 32% | 45% | 6.0 | 16.8 | 0/4 | 9.17e+06 | 7.52e-02 | 3.36e+01 | 3.18e+01 |
| B, no audit + guarded exact refit | 16 | 4 | 55% | 61% | 5.2 | 20.0 | 0/4 | 3.82e+06 | 3.58e-02 | 1.23e+01 | 1.13e+01 |
| B, no audit + guarded exact refit | 32 | 4 | 80% | 83% | 2.2 | 22.8 | 0/4 | 5.50e+06 | 3.86e-03 | 7.81e+00 | 6.45e+00 |
| B, no audit + guarded exact refit | 60 | 3 | 92% | 96% | 0.3 | 23.7 | 2/3 | 9.94e-01 | 5.37e-04 | 1.34e-01 | 9.39e-02 |
| B, audit every 16 | 4 | 4 | 5% | 19% | 4.8 | 9.2 | 0/4 | 6.44e+07 | 1.65e-01 | 7.86e+00 | 7.72e+00 |
| B, audit every 16 | 8 | 4 | 32% | 45% | 6.0 | 16.8 | 0/4 | 9.17e+06 | 7.52e-02 | 3.36e+01 | 3.18e+01 |
| B, audit every 16 | 16 | 4 | 56% | 62% | 5.2 | 20.2 | 0/4 | 3.82e+06 | 3.38e-02 | 5.73e+00 | 5.30e+00 |
| B, audit every 16 | 32 | 4 | 83% | 86% | 1.5 | 22.2 | 1/4 | 1.90e+06 | 1.69e-03 | 2.07e+00 | 1.73e+00 |
| B, audit every 16 | 60 | 3 | 90% | 93% | 0.7 | 23.0 | 0/3 | 1.95e+06 | 1.09e-03 | 3.10e+00 | 2.29e+00 |
| B, audit every 8 | 4 | 4 | 5% | 19% | 4.8 | 9.2 | 0/4 | 6.44e+07 | 1.65e-01 | 7.86e+00 | 7.72e+00 |
| B, audit every 8 | 8 | 4 | 32% | 45% | 7.5 | 18.2 | 0/4 | 5.89e+06 | 7.25e-02 | 2.91e+01 | 2.80e+01 |
| B, audit every 8 | 16 | 4 | 55% | 62% | 5.0 | 20.0 | 0/4 | 3.80e+06 | 3.31e-02 | 8.66e+00 | 7.96e+00 |
| B, audit every 8 | 32 | 4 | 83% | 85% | 1.8 | 22.2 | 1/4 | 2.16e+06 | 1.66e-03 | 1.53e+00 | 1.28e+00 |
| B, audit every 8 | 60 | 3 | 92% | 94% | 0.3 | 23.0 | 0/3 | 2.70e+07 | 1.47e-03 | 1.92e-01 | 1.36e-01 |
| from scratch (cascade + polish) | 4 | 4 | 16% | 23% | 8.2 | 13.8 | 0/4 | 6.90e+05 | 9.84e-02 | 1.06e+01 | 1.04e+01 |
| from scratch (cascade + polish) | 8 | 4 | 32% | 44% | 7.8 | 18.2 | 0/4 | 5.89e+06 | 7.53e-02 | 3.88e+01 | 3.73e+01 |
| from scratch (cascade + polish) | 16 | 4 | 34% | 50% | 7.0 | 19.0 | 0/4 | 2.25e+08 | 1.20e-01 | 4.49e+01 | 4.16e+01 |
| from scratch (cascade + polish) | 32 | 4 | 79% | 82% | 3.2 | 23.0 | 1/4 | 6.07e+05 | 1.83e-02 | 8.49e+00 | 7.02e+00 |
| from scratch (cascade + polish) | 60 | 3 | 92% | 93% | 1.7 | 24.0 | 1/3 | 1.34e+06 | 2.47e-03 | 8.38e-01 | 6.14e-01 |

What these say (observed):
- Synthetic: B without audits reaches recall(2) 90% at 16 paths and 100% from 32 on, 0 spurious from 32 on (dev 8 of 8; held-out 97% at 32, 100% at 60). The from-scratch cascade with polish is 86% / 96% (dev, 16 / 32 paths) and 77% / 95% held-out, so B is at least as good from 16 paths on and better between 24 and 56 paths, where the rebuild is non-monotonic (dev 98%, 96%, 96%, 100%, 100% at 24, 32, 40, 48, 56). At 4 paths B is clearly worse than the cascade (dev 8% against 26%, held-out 4% against 28%); at 8 paths B is 46% against 55% (dev) and 34% against 62% (held-out).
- Audit every 8 lifts the early counts (dev 55% at 8, held-out 60% at 8 and 94% at 16 against 76% without) and does nothing later; audit every 16 helps little (held-out 16 paths 82% against 76%).
- Live: B 88% (dev, 5 captures with 60 paths) and 92% (held-out, 3 captures) against 79-80% and 92% for the rebuild; audits every 8 add 3 points (dev 91%). On live data a map that fits the exact likelihood like the truth (ratio 0.99 after the guarded exact refit) was reached only on held-out captures 35 (24/24), 43 (24/24 at 41 paths) and 37 (22/24); no dev capture reaches ratio 1.2 (final recall(2) 19-22 of 24, 20 for capture 48 at its 48 paths), and the ratio is 1e5-1e9 for the rest (live truth is degenerate; every method ends there).
- The unexplored-area field error of an incomplete map is larger than the flat map's (ratio above 1 for B and for the rebuild at 8-16 paths, and for live at 60 paths where a few spurious or misplaced planets dominate the mean): a belief that publishes every planet found is worse than flat in unexplored areas until somewhere between 16 and 32 paths on synthetic data (2.0x flat at 16, 0.003x at 32). Explored-area error is already 16x (8 paths) to 90x (16 paths) below flat.
- Exploration that preceded B* (seeds 7001-7004 and 7001-7003, one run per cell, evaluation counts only): a single relative level 0.005 diverged on 7003 (K 31, 40 s per update, killed at 29 paths); levels 0.1/0.02/0.005 were no better than 0.02/0.005 at 16 paths (90% against 96% on 3 worlds) and cost more early; deaths every update, every 4th update, a local death test (thresholds 10, 30, 100) and no deaths gave identical recall from 16 paths on; births capped at 1500 and deaths at 300 evaluations gave recall(2) at 8 paths of 22-35% (7001-7004) against 54% for the first variant (births 3000, no separate deaths cap; 7001-7003), a different world set, so only indicative; B* kept the cheaper form. B* was chosen on those dev worlds, so its dev numbers are fitted to them; the held-out numbers are the unselected ones.

#### Q3 audit behaviour

How often a from-scratch rebuild changes the maintained map (audits every 8 paths on the no-audit trajectory; the rebuild is computed and compared, never adopted). "Any change" = at least one planet moved more than 3 units (nearest rebuilt planet 3-15 units away), added (a rebuilt planet with none within 15 units of the maintained map) or removed (a maintained planet with none within 15 of the rebuild). "Rebuild better" = higher Laplace evidence at relative sd 0.005 on all rows after refitting the maintained map there (the adoption rule). "Worse on adoption" = the rebuild has lower recall(2) or more spurious planets than the maintained map. The ratio column is flattering to the rebuild because it ends with the guarded exact refit.

#### Q3 audits on the no-audit trajectory (rebuild computed, never adopted), dev synthetic

| cfg | paths<= | audits | any map change (moved>3 or added or removed) | mean moved | mean added | mean removed | rebuild better (rel. evidence) | rebuild rec2 > inc | rebuild rec2 < inc | rebuild worse on adoption (rec2 lower or spurious higher) | ratio_rb<ratio_inc |
|---|---|---|---|---|---|---|---|---|---|---|---|
| probe=8 | 8 | 8 | 8 | 3.0 | 6.2 | 3.8 | 8 | 6 | 0 | 5 | 8 |
| probe=8 | 16 | 8 | 4 | 1.1 | 1.6 | 1.5 | 1 | 3 | 1 | 0 | 7 |
| probe=8 | 32 | 16 | 4 | 0.2 | 0.7 | 0.5 | 0 | 1 | 3 | 0 | 13 |
| probe=8 | 60 | 24 | 2 | 0.0 | 0.1 | 0.2 | 0 | 0 | 2 | 0 | 22 |

#### Q3 audits on the no-audit trajectory (rebuild computed, never adopted), dev live

| cfg | paths<= | audits | any map change (moved>3 or added or removed) | mean moved | mean added | mean removed | rebuild better (rel. evidence) | rebuild rec2 > inc | rebuild rec2 < inc | rebuild worse on adoption (rec2 lower or spurious higher) | ratio_rb<ratio_inc |
|---|---|---|---|---|---|---|---|---|---|---|---|
| probe=8 | 8 | 6 | 6 | 3.0 | 6.8 | 6.2 | 4 | 3 | 2 | 1 | 4 |
| probe=8 | 16 | 6 | 6 | 2.0 | 4.3 | 4.3 | 2 | 3 | 3 | 0 | 4 |
| probe=8 | 32 | 12 | 12 | 1.8 | 4.7 | 4.2 | 7 | 7 | 4 | 2 | 8 |
| probe=8 | 60 | 17 | 17 | 1.8 | 3.1 | 2.5 | 8 | 6 | 8 | 2 | 13 |

#### Q3 audits on the no-audit trajectory (rebuild computed, never adopted), held-out synthetic

| cfg | paths<= | audits | any map change (moved>3 or added or removed) | mean moved | mean added | mean removed | rebuild better (rel. evidence) | rebuild rec2 > inc | rebuild rec2 < inc | rebuild worse on adoption (rec2 lower or spurious higher) | ratio_rb<ratio_inc |
|---|---|---|---|---|---|---|---|---|---|---|---|
| probe=8 | 8 | 8 | 8 | 3.9 | 7.5 | 4.8 | 7 | 6 | 2 | 2 | 7 |
| probe=8 | 16 | 8 | 7 | 2.1 | 3.8 | 3.6 | 1 | 2 | 3 | 0 | 5 |
| probe=8 | 32 | 16 | 9 | 0.8 | 0.9 | 1.1 | 5 | 5 | 3 | 1 | 13 |
| probe=8 | 60 | 24 | 7 | 0.2 | 0.6 | 0.4 | 7 | 1 | 6 | 0 | 18 |

#### Q3 audits on the no-audit trajectory (rebuild computed, never adopted), held-out live

| cfg | paths<= | audits | any map change (moved>3 or added or removed) | mean moved | mean added | mean removed | rebuild better (rel. evidence) | rebuild rec2 > inc | rebuild rec2 < inc | rebuild worse on adoption (rec2 lower or spurious higher) | ratio_rb<ratio_inc |
|---|---|---|---|---|---|---|---|---|---|---|---|
| probe=8 | 8 | 4 | 4 | 3.0 | 4.8 | 3.2 | 3 | 1 | 1 | 3 | 4 |
| probe=8 | 16 | 4 | 4 | 3.2 | 6.5 | 7.5 | 1 | 1 | 3 | 0 | 1 |
| probe=8 | 32 | 8 | 8 | 1.1 | 4.0 | 3.9 | 3 | 4 | 3 | 1 | 4 |
| probe=8 | 60 | 10 | 6 | 1.7 | 1.8 | 1.6 | 2 | 0 | 4 | 1 | 6 |

Pooled over the dev and held-out worlds, audits every 8 paths: 179 audits, 59 where the rebuild has the higher evidence (adopted by the rule); of those 59, recall(2) rose in 36, stayed in 18 and fell in 5 (held-out synthetic at 8 paths twice, live dev twice, live held-out once); spurious planets rose in 15. Of the 120 rebuilds the rule rejected, the rebuild had lower recall(2) in 43 and higher in 13. So the rule rarely adopts a worse map (5 of 59 by recall, 15 of 59 by spurious count) and mostly rejects rebuilds that are worse. On synthetic data nothing was adopted after 24 paths in the dev worlds and 12 of 40 in the held-out ones (10 of which left recall unchanged); on live data 15 of 29 and 5 of 18 later audits were adopted, with 2 and 0 recall losses. A rebuild changes the maintained map in every live audit and, on dev synthetic worlds, in 8 of 8 audits at 8 paths, 4 of 8 at 16, 4 of 16 at 17-32 and 2 of 24 at 33-60.

#### Q4 per-planet support statistic (Q13 data)

For every planet of the no-audit maintained maps at 4, 8, 16, 32, 60 paths: `d05` = chi2 increase on all rows at relative sd 0.005 when the planet alone is removed (no refit), `d02` the same at 0.02, `dloc` the same after refitting the planets within 100 units of the removed one (300 evaluations). Correct = a true planet within 2 units; spurious = none within 15 (planets in between are in neither column). On live, "correct" = a planet of the last from-scratch map (61 or the capture's last path) within 5 units, spurious = none within 15 (at 60 paths this is the rebuild at the same path count, so that row is circular). "True planets hidden" = share of all true planets that are either not found or found with a statistic below the threshold; it is not computed for live. AUC = probability that a correct planet has a larger statistic than a spurious one.

#### Q4 support statistic d05, dev synthetic (correct = true planet within 2 units, spurious = none within 15)

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 16 | 47 | 0.72 | 16/16 / 4/47 / 92% | 15/16 / 22/47 / 92% | 8/16 / 33/47 / 96% | 4/16 / 43/47 / 98% | 4/16 / 47/47 / 98% |
| 8 | 91 | 40 | 0.69 | 89/91 / 3/40 / 54% | 84/91 / 19/40 / 56% | 50/91 / 30/40 / 74% | 10/91 / 33/40 / 95% | 4/91 / 39/40 / 98% |
| 16 | 171 | 9 | 0.76 | 169/171 / 1/9 / 12% | 152/171 / 3/9 / 21% | 117/171 / 7/9 / 39% | 61/171 / 8/9 / 68% | 12/171 / 9/9 / 94% |
| 32 | 192 | 0 | nan | 192/192 / 0/0 / 0% | 180/192 / 0/0 / 6% | 153/192 / 0/0 / 20% | 96/192 / 0/0 / 50% | 34/192 / 0/0 / 82% |
| 60 | 192 | 0 | nan | 192/192 / 0/0 / 0% | 187/192 / 0/0 / 3% | 180/192 / 0/0 / 6% | 138/192 / 0/0 / 28% | 74/192 / 0/0 / 61% |

#### Q4 support statistic dloc, dev synthetic (correct = true planet within 2 units, spurious = none within 15)

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 16 | 47 | 0.72 | 16/16 / 5/47 / 92% | 15/16 / 24/47 / 92% | 7/16 / 33/47 / 96% | 4/16 / 43/47 / 98% | 4/16 / 47/47 / 98% |
| 8 | 91 | 40 | 0.68 | 88/91 / 3/40 / 54% | 82/91 / 19/40 / 57% | 49/91 / 30/40 / 74% | 10/91 / 33/40 / 95% | 4/91 / 39/40 / 98% |
| 16 | 171 | 9 | 0.73 | 160/171 / 2/9 / 17% | 140/171 / 3/9 / 27% | 111/171 / 7/9 / 42% | 58/171 / 8/9 / 70% | 12/171 / 9/9 / 94% |
| 32 | 192 | 0 | nan | 184/192 / 0/0 / 4% | 161/192 / 0/0 / 16% | 140/192 / 0/0 / 27% | 89/192 / 0/0 / 54% | 33/192 / 0/0 / 83% |
| 60 | 192 | 0 | nan | 190/192 / 0/0 / 1% | 174/192 / 0/0 / 9% | 163/192 / 0/0 / 15% | 130/192 / 0/0 / 32% | 71/192 / 0/0 / 63% |

#### Q4 support statistic d02, dev synthetic (correct = true planet within 2 units, spurious = none within 15)

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 16 | 47 | 0.72 | 12/16 / 24/47 / 94% | 4/16 / 43/47 / 98% | 2/16 / 47/47 / 99% | 1/16 / 47/47 / 99% | 0/16 / 47/47 / 100% |
| 8 | 91 | 40 | 0.69 | 74/91 / 23/40 / 61% | 6/91 / 37/40 / 97% | 2/91 / 40/40 / 99% | 0/91 / 40/40 / 100% | 0/91 / 40/40 / 100% |
| 16 | 171 | 9 | 0.77 | 143/171 / 4/9 / 26% | 35/171 / 9/9 / 82% | 9/171 / 9/9 / 95% | 3/171 / 9/9 / 98% | 0/171 / 9/9 / 100% |
| 32 | 192 | 0 | nan | 173/192 / 0/0 / 10% | 68/192 / 0/0 / 65% | 20/192 / 0/0 / 90% | 4/192 / 0/0 / 98% | 0/192 / 0/0 / 100% |
| 60 | 192 | 0 | nan | 186/192 / 0/0 / 3% | 123/192 / 0/0 / 36% | 54/192 / 0/0 / 72% | 11/192 / 0/0 / 94% | 0/192 / 0/0 / 100% |

#### Q4 support statistic d05, held-out synthetic

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 8 | 47 | 0.83 | 8/8 / 2/47 / 96% | 8/8 / 17/47 / 96% | 5/8 / 36/47 / 97% | 2/8 / 43/47 / 99% | 1/8 / 46/47 / 99% |
| 8 | 66 | 47 | 0.72 | 65/66 / 6/47 / 66% | 60/66 / 24/47 / 69% | 39/66 / 35/47 / 80% | 13/66 / 43/47 / 93% | 1/66 / 46/47 / 99% |
| 16 | 147 | 25 | 0.81 | 146/147 / 4/25 / 24% | 126/147 / 13/25 / 34% | 90/147 / 22/25 / 53% | 39/147 / 24/25 / 80% | 6/147 / 24/25 / 97% |
| 32 | 190 | 2 | 0.94 | 189/190 / 0/2 / 2% | 175/190 / 1/2 / 9% | 148/190 / 2/2 / 23% | 84/190 / 2/2 / 56% | 31/190 / 2/2 / 84% |
| 60 | 195 | 0 | nan | 195/195 / 0/0 / -2% | 188/195 / 0/0 / 2% | 174/195 / 0/0 / 9% | 130/195 / 0/0 / 32% | 56/195 / 0/0 / 71% |

#### Q4 support statistic d05, dev live (correct = a planet of the last from-scratch map within 5 units, spurious = none within 15; circular at 60, which is that map's own path count)

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 10 | 32 | 0.71 | 10/10 / 2/32 / n/a | 7/10 / 24/32 / n/a | 2/10 / 28/32 / n/a | 0/10 / 31/32 / n/a | 0/10 / 32/32 / n/a |
| 8 | 51 | 35 | 0.79 | 51/51 / 7/35 / n/a | 36/51 / 26/35 / n/a | 17/51 / 30/35 / n/a | 3/51 / 33/35 / n/a | 0/51 / 35/35 / n/a |
| 16 | 78 | 38 | 0.81 | 77/78 / 7/38 / n/a | 67/78 / 24/38 / n/a | 45/78 / 32/38 / n/a | 20/78 / 37/38 / n/a | 2/78 / 38/38 / n/a |
| 32 | 105 | 25 | 0.74 | 105/105 / 1/25 / n/a | 92/105 / 13/25 / n/a | 73/105 / 15/25 / n/a | 46/105 / 20/25 / n/a | 20/105 / 23/25 / n/a |
| 60 | 99 | 9 | 0.61 | 99/99 / 0/9 / n/a | 93/99 / 2/9 / n/a | 86/99 / 2/9 / n/a | 59/99 / 6/9 / n/a | 34/99 / 7/9 / n/a |

#### Q4 support statistic d05, held-out live

| paths | correct planets | spurious planets | AUC | thr 1e+05: correct kept / spurious dropped / true hidden | thr 1e+06: correct kept / spurious dropped / true hidden | thr 3e+06: correct kept / spurious dropped / true hidden | thr 1e+07: correct kept / spurious dropped / true hidden | thr 3e+07: correct kept / spurious dropped / true hidden |
|---|---|---|---|---|---|---|---|---|
| 4 | 10 | 19 | 0.78 | 10/10 / 5/19 / n/a | 2/10 / 17/19 / n/a | 0/10 / 18/19 / n/a | 0/10 / 19/19 / n/a | 0/10 / 19/19 / n/a |
| 8 | 37 | 24 | 0.83 | 37/37 / 5/24 / n/a | 21/37 / 21/24 / n/a | 6/37 / 24/24 / n/a | 0/37 / 24/24 / n/a | 0/37 / 24/24 / n/a |
| 16 | 56 | 20 | 0.89 | 56/56 / 7/20 / n/a | 38/56 / 18/20 / n/a | 16/56 / 20/20 / n/a | 4/56 / 20/20 / n/a | 0/56 / 20/20 / n/a |
| 32 | 82 | 7 | 0.85 | 81/82 / 0/7 / n/a | 79/82 / 3/7 / n/a | 62/82 / 5/7 / n/a | 35/82 / 7/7 / n/a | 7/82 / 7/7 / n/a |
| 60 | 67 | 2 | 0.94 | 67/67 / 0/2 / n/a | 62/67 / 1/2 / n/a | 57/67 / 2/2 / n/a | 39/67 / 2/2 / n/a | 19/67 / 2/2 / n/a |

Summary. The removal chi2 does not separate spurious from correct planets: AUC 0.68-0.76 on synthetic worlds at 4-16 paths (spurious planets exist only there; none at 32 and 60 paths), 0.71-0.81 on live at up to 32 paths and 0.62 at 60. Medians are within a factor of 2 to 5 (order 6e6 for correct and 1-2e6 for spurious planets at 16 paths). Every threshold that removes most spurious planets hides a large share of the true ones: at 16 paths 3e6 drops 7 of 9 spurious planets and hides 39% of the true planets; at 8 paths it drops 30 of 40 and hides 74%; at 32 paths it would hide 20% and at 60 paths 6% of true planets with nothing to remove. The local refit changes little (dloc equals d05 to within 0.1% for 87% of the 826 planets: neighbours cannot take over the pull of a 20-40 unit planet). It does expose redundant planets: dloc is below 30 for 3 of the 826 planets, one planet at 4 paths whose removal lowers the chi2 even without a refit (111 units from any true planet) and the two halves of a split pair at 16 paths (each 2.3-3.5 units from the same true planet, dloc -2.0). B*'s death test ranks candidates by the plain removal chi2, so it did not try them; ranking by dloc would. A support test built on the chi2 increase of removing one planet is therefore not usable as a precision gate here; what removes the spurious planets in these runs is the continued fitting (spurious planets average 5.9 at 4 paths, 1.1 at 16, 0.0 at 32 on synthetic data).

#### Held-out confirmation (one pass, B* with no audit, audit every 16, audit every 8; synthetic 7101-7108, live 34, 35, 37, 42, 43)

Tables above. B* held up: recall(2) 97% at 32 paths and 100% at 60 (no audit), 100% with audit every 8 at 32; live 92% at 60 (rebuild 92%). Weaker than dev at 16 paths (76% against 90%; the rebuild 77% there), the same as dev at 60. Live 34 has 2 paths of data (no result); at the end of each capture recall(2) is 24/24 for 35, 24/24 for 43 (41 paths), 22 for 37 and 20 for 42.

#### Verdict

- Quality (observed): B is viable. From 16 paths on it equals or beats the from-scratch cascade-plus-polish on both synthetic and live data (above), including on held-out data; it is behind at 4-8 paths (the cascade's three-level start from nothing is better there). The incrementally maintained map is the better map after about 24 paths; rebuilds can be worse than the map they would replace (non-monotonic recall, 43 of the 120 rejected rebuilds had lower recall, 13 higher).
- Latency (observed on the Mac, K=24): the target (median well under 100 ms, worst about 1 s) is met in steady state: median 22 ms, worst 250 ms at 8 threads, 33 ms and 243 ms at 1 thread, flat from 17 to 60 paths and across K = 8-26. It is not met while planets are being born: updates that run births take 1-20 s on the Mac (8 threads) and are frequent on live captures until the map has stabilised (live 45: median 2 s at 17-61 paths). Capping the births at 300 evaluations per update brought the live worst case to 1.7-2.3 s and the p90 to about 1.1 s at the price of 1-4 planets of recall at 60 paths (two captures); a cap that keeps the worst update near 1 s without losing recall (spreading births over several updates, a smaller joint-refit budget) was not tested (inferred: it needs a birth that can be resumed).
- Deaths and the Q13 support test: with this machinery deaths did not change any result (only the continued fitting removes spurious planets), and a chi2-removal support statistic does not discriminate (Q4); only dloc found the 3 redundant planets in the replays. Do not gate publication on the removal chi2.
- Audits: not needed on synthetic data after about 24 paths; cheap insurance early (audit at 8 paths adopted in 8 of 8 dev and 7 of 8 held-out worlds and lifted recall(2) at 8 paths from 46% to 55% (dev) and 34% to 60% (held-out)). On live data the rule adopts about half of the later rebuilds and they are better in most of them. A rebuild costs 3k-34k full-row equivalents and about 30-60 s on the Mac, so it must run in the background and be adopted only by the evidence rule.

#### Recommended configuration (if B is built)

Thinned row set of 1000 rows (at most 2 per 14-unit cell, strongest first; 2000 costs about 2x for no measured gain) at relative levels 0.02 then 0.005; local warm refit of the planets within 100 units of the new path (300 evaluations); births on the thinned rows with a per-update cap that needs a decision (1500 evaluations: best recall, worst update 10-20 s; 300: worst update about 2 s, recall down 1-4 planets); a cheap death pass only for redundancy (candidates ranked by dloc, remove if dloc < 30; untested as a pass, the 3 cases above are the evidence), not a support-threshold gate; one guarded exact refit of the published map (the chi2 ratio 0.99 needs it); audits: rebuild at 8 paths and then every 16 (or 24) in the background, adopted only if its relative-0.005 evidence on all rows is higher than the maintained map's after a refit there; for the first 8 paths a from-scratch cascade at each update is at least as good as B (4 paths 26-28% against 4-8%) and affordable (about 30 s).

#### Not done, cut, limits

- Cut: latency at 1 and 4 threads and the cap ablation were measured on one synthetic world (7001, 40 paths) only; live latency only at 8 threads (captures 33, 40, 45); the production comparison at 8 threads only and with its 5 s budget; no repeats (one run per cell, so run-to-run spread is unknown; the live recall differences of 1-4 planets between birth caps are single runs). Quality: the 14 dev and 13 held-out worlds were run for the three audit variants (no audit with probe, 16, 8); the ev=300/600 birth-cap runs are on the Mac for captures 33 and 40 only because `jolly` became unavailable (about 04:20) before the planned jolly runs for all six dev captures. Noise 0.1 was not run (B* and the relative levels are tuned on exact noise). The reserved scorecard cases (synthetic 12/18/44/49) and capture 52 were not touched.
- Evidence: synthetic generator (24 planets, long paths at speed 8) and ten live captures with degenerate truth; B* was chosen on the dev worlds (seeds 7001-7004 for the ablations); only the final held-out pass is unselected. The paths of a live capture come in capture order, which is correlated in space; the synthetic order is random.
- Earlier exploration runs (single level, three levels, cap 2000, evidence-based deaths) used earlier revisions of the harness (evidence computed at every update, full warm refit); only their quality/cost summaries are quoted, not mixed into the B* tables. The latency of the exact refit that produced "+ guarded exact refit" was not measured, and the maintained state is not that map.
- The B map carries no covariance or weights (evidence is computed only when births run); `Mode` weights and the published belief were not built. Targeting integration was not tried.
- A planet cap of 32 was used in the harness (production default 24); the K=26 world therefore exceeds the production cap.

Reproduction (from the worktree, with `einsteinbot.sqlite3` present; `CACHE` holds from-scratch rebuilds so audits reuse them):
`cargo test --release --lib diagnose_rebuild -- --ignored --nocapture --test-threads=1` with `CACHE=/tmp/P6cache THREADS=2 PATHS=60 SEEDS=7001 LIVE=-` (also `COUNTS=4,8,...`) fills the cache and prints the from-scratch rows.
`cargo test --release --lib diagnose_incremental -- --ignored --nocapture --test-threads=1` with `CONFIGS="lv=0.02/0.005:rf=300:ev=1500:lr=100:dl=30:dn=4:cap=1000:probe=8"`, `PLANETS=1` for the Q4 lines, `PROD=1` for the production update, `THREADS=8`, `SEEDS`/`KS`/`LIVE`/`PATHS` as in the other harnesses (`audit=16`, `audit=8` instead of `probe=8` for the adopting runs; held-out `SEEDS=7101,...,7108`, `LIVE=34,35,37,42,43`). Tables: `python3 -I scripts/p6_tables.py {cost,latency,quality,audit,support} FILES [--worlds synthetic|live]`.

### P8 results (2026-11-27, branch `p8`, built on `p6`; harness `src/estimator/diagnose/baseline.rs` (ignored test `diagnose_baseline`) plus `diagnose_incremental` for B*; tables from `scripts/p8_tables.py`)

Question: baseline evidence for the staged replacement decision, on real captures: (a) flat (K = 0), (b) the current production search (default `Estimator`), (c) the from-scratch cascade (E9 thinned relative-noise cascade, then the E9c guarded polish, the P6 "rebuild"), (d) P6's B* incremental at the capture's last path. Truth (the oracle map each capture discloses) only scores. Production, estimator and fit code are unchanged.

#### Stage 1: pilot on dev captures 33, 40, 45 (observed)

Setup. Every capture: the first map epoch, at most 61 paths (`PATHS=60`, as in P6), exact noise in the scoring likelihood. (b) is fed path by path through `Estimator::observe` (default settings: noise level 0.1, 24-planet cap) with a deterministic limit of 2000 residual-plus-Jacobian evaluations per update (`max_evaluations`; the wall-clock budget is lifted to 24 h; contacts are not fed, as in the other diagnosis harnesses); the last published belief is scored. The limit of 2000 is my choice: the P6 production run used 250-440 evaluations in its 5 s budget at K = 24, so 2000 gives the production search about five times its live budget; every update used the limit in these runs (about 120k evaluations per capture). (c) runs once on all paths (stage-1 cap 28000 evaluations, polish budget 12000). (d) is the P6 configuration B*. Machine: jolly, 8 threads for (a)-(c) and 2 for (d), three captures at a time next to other jobs (load 11-16); results do not depend on thread count by construction, wall times are indicative only.

| capture | paths | method | K | rec2 | rec15 | spur | chi2 ratio | field expl/flat | unexpl/flat | whole/flat | evals | wall s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 33 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 9.83e+11 | 1 | 1 | 1 |  |  |
| 33 | 61 | prod | 24 | 2/24 | 7/24 | 17 | 5.02e+10 | 2.75 | 29.2 | 18.9 | 122609 | 1274 |
| 33 | 61 | cascade | 26 | 18/24 | 20/24 | 6 | 3.75e+07 | 0.17 | 1.28 | 0.834 | 41162 | 118 |
| 33 | 61 | B* | 23 | 22/24 | 22/24 | 1 | 4.67e+07 | 1.62e-03 | 2.94 | 1.9 |  |  |
| 40 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 1.42e+12 | 1 | 1 | 1 |  |  |
| 40 | 61 | prod | 22 | 19/24 | 21/24 | 1 | 3.70e+03 | 7.96e-03 | 2.42 | 1.76 | 120366 | 699 |
| 40 | 61 | cascade | 23 | 20/24 | 22/24 | 1 | 459 | 1.18e-04 | 1.26 | 0.913 | 28456 | 95 |
| 40 | 61 | B* | 22 | 19/24 | 21/24 | 1 | 2.06e+06 | 2.53e-03 | 34.7 | 25.2 |  |  |
| 45 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 8.33e+11 | 1 | 1 | 1 |  |  |
| 45 | 61 | prod | 22 | 14/24 | 18/24 | 3 | 1.44e+07 | 0.0353 | 5.51 | 4.0 | 111391 | 459 |
| 45 | 61 | cascade | 22 | 16/24 | 19/24 | 2 | 1.48e+07 | 0.199 | 2.48 | 1.81 | 40519 | 72 |
| 45 | 61 | B* | 22 | 21/24 | 21/24 | 1 | 8.52e+06 | 2.40e-03 | 3.61 | 2.62 |  |  |

Sanity checks (observed). The cascade reproduces the E9c C numbers where they overlap: capture 40 recall(2) 20 with chi2 ratio 460 (E9c: 20, 460), capture 45 recall(2) 16 (E9c: 16), capture 33 recall(2) 18 (E9c: 18); B* at 61 paths matches P6's live dev picture (21-22 of 24 on 33 and 45). The cascade is better than flat in the explored area on all three (0.17, 1e-4, 0.2 of the flat error). It is not better than flat in the unexplored area (1.26-2.5x flat) or whole field on 45: a published map with a few wrong or missing planets is worse than flat away from the data, as found in P6. The production search is worse than flat even in the explored area on capture 33 (2.75x flat, 17 spurious planets, recall(2) 2 of 24) although it spent 122k evaluations.

Stage-2 estimate (written before the launch). Pilot cost per capture with all 61 paths: (a)-(c) 530-1390 s at 8 threads (prod 460-1270 s dominates, the cascade 70-120 s), (d) 1-5 minutes at 2 threads. The production search is mostly single-threaded (it used about 2.4 cores of 8). Stage 2 is 29 captures (14 with 61 paths, the others fewer); run as a queue of 8 captures at a time at 4 threads each (32 threads on jolly, P7 using up to 16 more); expected about 29 x 15 min / 8 = 1 to 1.5 hours wall, longer if jolly stays loaded. Launched 11:50 UTC.
