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

E9c (added after the user's clarification, 2026-11-26): the user meant the relative
error bounds as in newton-bot (sigma growing with |a|, a tolerance for what the model does
not capture) when asking for "uncertainty bounds" on the fit; the session had read it as
parameter covariance. So also test a relative-error term as part of the permanent
likelihood (not only in the search), with an allowance that shrinks as the map becomes
complete (more planets explain more of the field), and compare final planet position
error, chi2 ratio, explored/unexplored RMSE and evidence behaviour against the exact
likelihood. This also bears on Q8 (the provisional noise level 0.1 is a crude form of the
same allowance). Status: todo (queued after W5 reports; W5's brief covers search-only use).

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

### P5 round 1 results (2026-11-26; session summary of worker reports; details in each worktree's copy of this file)
Worktrees (branches `worktree-wf_3d9f39da-929-N`, not merged; production search unchanged): W1 `-1` (E1/E7), W2 `-2` (E2, stalled, relaunched), W3 `-3` (E3/E4/E6), W4 `-4` (E5/E8), W5 `-5` (E9). Dev worlds only unless stated. Baseline greedy from flat: recall(2) about 0, chi2 ratio 1e8-1e10.
- **E9 thinned rows + relative noise cascade (W5): adopt as an opt-in search stage, with limits.** Growth on thinned rows (<=2 per 14-unit cell, strongest first; caps 500/1000/2000 rows) with a relative sd term (0.1/0.02/0.005 of |a|), then exact refit, exact growth and deaths. Growth stage alone with 60 long paths: synthetic dev recall(2) 100% (0.0 spurious), held-out 97% (0.4), live dev 82% (2.8), at about 2k equivalent full-row evaluations (greedy: ~0% at 60k). Full pipeline: synthetic dev 8/8, held-out 6/8 (target needs 7), live dev 1/6, live held-out 3/4 usable. Tuned on dev worlds. Weak points: exact polish drifts from incomplete maps (7107); 8-path worlds only 58% synthetic / 38% live growth-only; noise 0.1 unfinished. Ablation: row selection matters more than the noise form (all rows with marginal variance gets 1-4 of 24).
- **E1 free-space / E7 non-overlap (W1): reject as search fixes; keep free-space at radius 20 and non-overlap as cheap candidate pre-filters.** Server rule verified (shot ends at first substep within radius; planets never overlap); production already has a tick hinge. True planets pass the free-space test 24/24; filter rejects 35% of wrong seeds and 6-47% of right-ish seeds depending on radius used (use radius 20).
- **E3/E4/E6 (W3): reject as seed generators.** Pull-direction/vote/triangulation seeds reach 96% within 15 units only with a near-exact background (flat: 15%; 10% of other planets unknown: 18%). Contact ring/pull seeds 23-29% within 15, no end-to-end gain. Possible late-stage proposal for a nearly complete map (untested).
- **E5 repair moves (W4): reject.** No variant reaches the target; gains are from extra evaluations (random-deletion control matches), rebuilds from empty land in the same mode.
- **E8 (W4): stall rule 100 evaluations / 1e-3 relative gain: adopt as option** (about 30% fewer refit evaluations, nearly equal chi2); normal-equations LM reject (different minima, max ratio 1e4); radius-column shortcut reject (cannot construct; at most 5.5%).
- **E2 dictionary NNLS (W2): not finished** (worker stopped by an accidental tool rejection; code uncommitted in worktree `-2`); relaunched.
- **E9c permanent relative likelihood: relaunched** (builds on W5's branch), see below.
- **E2 dictionary NNLS (W2, branch `worktree-wf_3d9f39da-929-2`, merged with the E9 branch): inconclusive as a cheap pre-stage, reject as a replacement for or seed of the E9 growth stage.** Thinned-row dictionary (res 15, exact kernel, free-space filter on, batch of 20 per round, LM refit of peaks): dev synthetic recall(2) 92-98%, spurious 0.1-0.8, at about 100-300 full-row-equivalent evaluations (1011 for t2000 res10) against the cascade's 100% at ~2270; 5-9 s on one core for the light variant. Dictionary on all rows or strided rows is far worse (recall 34-65%) and 10x costlier; resolution 15 beats 22 and 30; OMP no better than batch. Held-out growth-only: synthetic dictionary 93% (1.9 spurious) vs cascade 97% (0.2); live 77% vs 93%. Dictionary-only never meets the chi2 target (1-2 planets missing or off); seeding the cascade with it did not fix captures 40/41/45 and the exact stage costs the same. 8 paths: ~42-49% vs cascade 43% (neither adequate). Inferred, not shown: recall is limited by grid quantisation vs exact-noise sharpness. Possible use: fast sanity screen or fallback. Not run: noise 0.1, K-aware evaluation counter (seeded runs start at K~28, so counts understate cost).
- **E9c relative allowance in polish (branch `e9c`, worktree `.claude/worktrees/e9c`, on top of the E9 branch): adopt a narrow form as an opt-in; inconclusive against the target.** After the cascade: growth and deaths on FULL rows at relative sd 0.005 then 0.001, then an exact refit accepted only if exact chi2 falls and (no start planet moves >3 units or chi2/row <= 1.5); NO exact growth/deaths (they cause the drift; across 42 cases E9-as-is ends below its stage-1 recall in 15, the guarded form in 1). Complete maps: all programs 24/24, 0 spurious, ratio 0.994-0.996; polish cost 1.5k vs 7.0k (full-row evaluations); stage 1 about 2.2-3k. Held-out synthetic 7101-7108: recall(2)>=90% 8/8 (was 6/8), chi2 ratio <=1.2 on 6/8 (7107, 7108 still miss 1-3 planets). Live held-out 37 and 42 keep chi2 ratios 1e6-1e7; 8-path and noise 0.1 remain far below target (e.g. 8 paths recall 12/18/4/11 of 24). A chi2-gated skip of the polish does not work (a complete map still has exact chi2/row 400-10000 before the refit). Relative evidence stops at K=24 on complete maps, overshoots on incomplete ones (spurious 3-6). Field RMSE explored/unexplored on 7107: 2e-2/5e-1 (guarded) vs 15/176 (E9 as is). Guard thresholds were set after dev live results; held-out run once (F seen twice). Some dev raw outputs lost to an rsync --delete (numbers from consoles); held-out intact in /tmp/E9c-out on jolly. Expected total cost 3.5-6k full-row equivalents on a complete map, up to ~15k incomplete.

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

### P7 results (2026-11-27, branch `p7`, built on `p6`; harness `src/estimator/diagnose/resumable.rs`, ignored test `diagnose_resumable`; tables from `scripts/p7_tables.py` and `scripts/p6_tables.py quality`)

Question: can the planet search be resumable, so that new paths join a running search instead of restarting it, and a best-estimate map is always available? Production search unchanged (option-gated, diagnosis harness only). Truth scores only; contacts create nothing. Short answer (grades: observed unless marked): the live-sized behaviour is good. A new path's map is published 50-350 ms after it arrives (B* blocks 1-122 s), the worst update (local refit plus one slice) is 291-341 ms on the Mac at 8 and at 10 threads, and under a path-every-500 ms schedule the final recall equals uncapped B* on the three synthetic worlds and is within 1-4 planets on live. Absorbing new information beats restarting only at very tight budgets; at 3-6 slices per path restarting is as good.

#### What was built (`Search`)

State: the paths at each relative level (0.02, 0.005; full rows, thinned per step to 1000 rows by the P6 rule), the published map, the level being searched, candidate planets (seed positions with their alone fits, at most 24, duplicates within 30 units dropped), partial joint refits (map plus a candidate, at most 3) and a pending death trial. `add_path` = the B* local refit (planets within 100 units of the new path, 300 evaluations, per level), then the map is published; pending candidates and partials are kept (`restart=1` drops them). `step(evals, deadline)` = a bounded slice: seeds from the unexplained windows, alone fits of unfitted candidates (batches of 8, 15-150 evaluations each), re-scoring of fitted candidates against the current rows and map (one chi2 evaluation each), up to 3 joint refits resumed from where the last slice left them (a refit is settled when it uses less than its budget, or after 6 rounds), adoption of the best child if its Laplace evidence beats the map's, then a death trial (the 2 planets whose removal costs least chi2, refit jointly, kept if the evidence rises, as the depth-1 prune of B*). A level ends when no birth improves the evidence. The slice stops at the evaluation limit (checked between batches, so it can overshoot by one batch) or the wall ceiling (also passed to the fits). Slice size 300 evaluations, ceiling 250-300 ms. Not built: from-scratch background audit on the resumable map (the harness can adopt the cached P6 rebuilds with `audit=N`; not run), and the early-update (<8 paths) from-scratch rule; the planet cap is 32 as in P6.

#### Live-sized behaviour (Mac, release; load average 2-4 from other sessions; one run per cell)

Arrival schedule: a new path every 500 ms or every 2 s; between arrivals the resumable search takes slices (300 evaluations, 250 ms ceiling, 8 threads); B* is the P6 blocking update (births cap 300 or 1500) on the same schedule, a backlog processed one path at a time. "Publish lag" = the worst time from a path's arrival to its map being published (B*: end of its update). Final = recall(2) (spurious) after the last event (61 paths on live, 60 synthetic, plus one period of tail). First correct = first planet within 2 units. t50 = time to half the final recall. The live 33, 500 ms resumable cell ran while I compiled on the Mac (its final 18 against 19 in a quiet standalone run), so it may be slightly pessimistic.

| world | period | method | final rec2 (spur) | t50 s | rec2 at 10 s / 20 s | worst publish lag ms |
|---|---|---|---|---|---|---|
| live 33 | 500 | resumable | 18 (3) | 6.5 | 7 / 19 | 200 |
| | | B* cap 300 | 18 (3) | 9.0 | 9 / 19 | 958 |
| | | B* cap 1500 | 22 (1) | 9.6 | 11 / 22 | 5582 |
| live 33 | 2000 | resumable | 16 (8) | 15.2 | 6 / 8 | 349 |
| | | B* cap 300 | 18 (3) | 32.7 | 0 / 1 | 932 |
| | | B* cap 1500 | 22 (1) | 20.3 | 3 / 7 | 4440 |
| live 40 | 500 | resumable | 17 (3) | 4.6 | 9 / 13 | 93 |
| | | B* cap 300 | 17 (4) | 13.5 | 6 / 14 | 1251 |
| | | B* cap 1500 | 19 (1) | 17.9 | 7 / 10 | 18554 |
| live 40 | 2000 | resumable | 16 (3) | 14.6 | 5 / 9 | 78 |
| | | B* cap 300 | 17 (4) | 50.6 | 1 / 4 | 1234 |
| | | B* cap 1500 | 19 (1) | 59.8 | 5 / 6 | 9729 |
| live 45 | 500 | resumable | 21 (1) | 6.0 | 7 / 15 | 50 |
| | | B* cap 300 | 13 (3) | 19.0 | 4 / 11 | 905 |
| | | B* cap 1500 | 21 (1) | 10.3 | 6 / 6 | 122382 |
| live 45 | 2000 | resumable | 21 (1) | 22.5 | 2 / 8 | 48 |
| | | B* cap 300 | 13 (3) | 74.5 | 0 / 1 | 877 |
| | | B* cap 1500 | 21 (1) | 28.3 | 1 / 4 | 89570 |
| synthetic 7001 | 500 | resumable | 24 (0) | 12.0 | 7 / 24 | 61 |
| | | B* cap 300 | 24 (0) | 7.2 | 18 / 24 | 1876 |
| | | B* cap 1500 | 24 (0) | 7.5 | 12 / 24 | 10161 |
| synthetic 7002 | 500 | resumable | 24 (0) | 16.0 | 0 / 18 | 247 |
| | | B* cap 300 | 24 (0) | 8.7 | 14 / 20 | 2069 |
| | | B* cap 1500 | 24 (0) | 6.2 | 24 / 24 | 4844 |
| synthetic 7003 | 500 | resumable | 24 (0) | 11.5 | 5 / 20 | 88 |
| | | B* cap 300 | 24 (0) | 11.3 | 9 / 20 | 8083 |
| | | B* cap 1500 | 24 (0) | 28.4 | 6 / 10 | 41370 |
| synthetic 7001-7003 | 2000 | resumable | 24 / 24 / 24 | 14.1 / 14.0 / 22.5 | 6 / 7 / 4 | 293 / 59 / 74 |
| | | B* cap 300 | 24 / 24 / 24 | 26.7 / 30.7 / 38.7 | 0 / 0 / 0 | 993 / 1204 / 1290 |
| | | B* cap 1500 | 24 / 24 / 24 | 16.0 / 15.8 / 39.8 | 4 / 3 / 0 | 5304 / 2675 / 28916 |

(Time to first correct planet is 0-2 s for every method on most worlds, so it does not separate them; the full timelines are in the raw `TL` lines.) Reading: the resumable search never blocks; its publish lag is the local refit (50-350 ms), where B* cap 300 blocks 0.9-8 s and cap 1500 up to 2 minutes (live 45, whose map stays incomplete). At 500 ms it reaches half of its final recall earlier than either B* variant on all three live worlds with a comparable final map; on the synthetic worlds B* is faster to half recall on 7001 and 7002 (7.2-7.5 s and 6.2-8.7 s against 12.0 and 16.0 s) and the resumable search is the one that does not block; at 2 s the cap-300 B* ends higher on live 33 and 40 (18 and 17 against 16 and 16) but is late to get there. Uncapped B* still ends 1-4 planets higher on live 33 and 40 and has fewer spurious planets (inferred cause: deeper search per path; not tested).

Per-update latency, resumable, 1 slice per path (300 evaluations, 250 ms ceiling), live 33 / 40 / 45, 60 paths, median / p90 / max ms of local refit plus slice:

| threads | live 33 | live 40 | live 45 |
|---|---|---|---|
| 8 (load 2-3) | 242 / 264 / 291 | 261 / 280 / 341 | 221 / 272 / 313 |
| 10 (load 4) | 252 / 264 / 298 | 261 / 269 / 341 | 205 / 275 / 314 |

Acceptance (worst update about 1 s or less): met, at 291-341 ms. The slice is cut by its 250 ms ceiling, so these numbers are the ceiling plus the refit (23-150 ms) and the work, not the algorithm's speed; 8 and 10 threads are indistinguishable (observed). I did not pin threads to P or E cores and cannot say anything about P/E effects (the Mac load from other sessions was 2-4). The Mac binary for these latency runs and the schedule runs is the build before the `dt` flag and work counters were added (same algorithm, death trial on).

#### Quality at live-sized budgets (deterministic: 300 evaluations per slice, wall ceiling off, jolly; evaluation counts do not depend on threads)

Recall(2) of the true planets at n paths (all dev worlds; `ns` = slices per path; absorb = pending work kept, restart = dropped at every path). Total evaluations over 60 paths per world: ns=1 about 18k; ns=3 about 34k synthetic / 47k live; B* cap 300 28k / 40k; B* cap 1500 44k / 63k.

| method | dev synthetic 16 / 32 / 60 paths | dev live 16 / 32 / 60 paths |
|---|---|---|
| resumable ns=1 | 4% / 33% / 76% | 0% / 10% / 48% |
| restart ns=1 | 0% / 0% / 0% | 0% / 0% / 0% |
| resumable ns=3 | 39% / 89% / 92% | 23% / 60% / 75% |
| restart ns=3 | 13% / 90% / 97% | 6% / 48% / 82% |
| resumable ns=6 | 61% / 100% / 100% | 44% / 66% / 89% |
| restart ns=6 | 85% / 97% / 100% | 42% / 74% / 87% |
| resumable ns=3, no death trial | 24% / 54% / 68% | 28% / 52% / 63% |
| B* cap 300 | 60% / 91% / 100% | 26% / 58% / 76% |
| B* cap 1500 (P6 output) | 90% / 100% / 100% | 51% / 72% / 88% |

After the 60th path, with slices continued until the search is idle (at most 150 slices): ns=1 99% synthetic / 78% live, ns=3 100% / 85% (live at 61 paths).

Absorbing against restarting (observed, dev): at one slice per path restarting never finishes a round and ends with an empty map (0%), while absorbing reaches 76% / 48% at 60 paths; so absorbing helps at tight budgets. From 3 slices per path on restarting is not worse (97% against 92% and 82% against 75% at ns=3; 85% against 61% at 16 paths and 100% against 100% at 60 for ns=6); absorbing's advantage there is at most 3 points at 32 paths. The mechanism is that candidate fits and joint refits are kept across paths, but at larger budgets a restart can redo them within one slice chain and also drops stale state (inferred). The death trial matters: without it K overshoots (25-26 planets) and recall at 60 paths drops from 92% to 68% (synthetic) and 75% to 63% (live) at ns=3.

#### Held-out (one pass; final config ns=6, 300 evaluations, deterministic; synthetic 7101-7108, live 34, 35, 37, 42, 43; live 34 has 2 paths, 43 has 41, so live rows have 4 worlds at 16-32 and 3 at 60)

| method | synthetic recall(2) 16 / 32 / 60, spurious at 60 | live 16 / 32 / 60, spurious at 60 |
|---|---|---|
| resumable ns=6 | 73% / 94% / 99%, 0.2 | 27% / 46% / 85%, 1.7 |
| restart ns=6 | 77% / 93% / 98%, 0.6 | 43% / 76% / 92%, 1.0 |
| B* cap 300 | 57% / 80% / 98%, 0.2 | 23% / 67% / 85%, 1.3 |
| B* cap 1500 (P6) | 76% / 97% / 100%, 0.0 | 55% / 80% / 92%, 0.3 |

Recall(2) at 60 paths is within the 1-2 planets asked for on synthetic data (99% against 100%, 0.2 planets) and 1.7 planets below uncapped B* on live (85% against 92%); it matches B* cap 300 (live 85%) while never blocking. Restart ns=6 is as good or better on live (92%), so the held-out data do not show an absorption benefit at that budget. The settle row (slices to idle, at most 150) is in the raw output only. The raw chi2 ratio after the run is 1e4-1e11 as in P6 (relative-level fit); the `+x` guarded exact refit rows are in the raw QUAL lines and were not tabulated.

#### Verdict

- Live behaviour (observed): the resumable search meets the update-latency target (worst 291-341 ms against about 1 s) and always has a fresh map (publish lag 50-350 ms against 1-122 s for B*). With a path every 500 ms its final recall equals B* cap 1500 on the three synthetic worlds (24/24 each; B* is faster to half recall on two of them) and is 18/17/21 against 22/19/21 on live 33/40/45, reaching half of it earlier than B* on all three live worlds. With a path every 2 s on live 33 it ends below B* (16 against 18-22) with 8 spurious planets.
- Quality at fixed per-path budget: one 300-evaluation slice per path is too little by itself (76% synthetic, 48% live at 60 paths); 6 slices per path matches B* cap 300 and is within 0-4 planets of uncapped B*. What the 500 ms schedule delivers is roughly 1.5-2 slices per path (inferred from wall time), and it does better than the 1-slice replay (24/24 on all three synthetic worlds) because the idle search keeps working on the final map.
- Absorbing new information helps only when the budget is too small to finish a round between paths; otherwise restart is equal. I would build the resumable form for the always-available map and the bounded update, not for the absorption itself.

#### Not done, cut, limits

- Cut: the from-scratch audit on the resumable map and the early-update (<8 paths) from-scratch rule (recall at 8 paths is 16% synthetic and 19% live for ns=6 against 46% and 27% for B* uncapped); noise 0.1; repeats (one run per cell, run-to-run spread unknown, and the wall-clock runs depend on load); P/E-core effects; schedule runs on synthetic worlds beyond 7001-7003 and on the held-out worlds; slice sizes other than 300 evaluations and ceilings other than 250-300 ms; `MAX_ROUNDS`, `BATCH`, candidate limits and the 30-unit duplicate radius are untuned. Reserved scorecard cases and capture 52 were not touched.
- Limits: the Mac latency and schedule runs are one process at a time at 8 or 10 threads with the stated load; jolly quality runs used 2 threads per process (load 20-30), thread-independent by construction. Live truth is degenerate (several maps tie), so live recall differences of 1-3 planets are within what P6 saw between configurations. A slice may exceed its evaluation limit by one batch.
- Local copies of the raw output: `/tmp/P7mac/{dev,dev2,b300,held,hb300,out}`; jolly `/tmp/P7/out/{dev_slice,dev_slice2,dev_b300,held_final,held_b300}` (and `dev_slice_nodeaths_partial`, an early killed run without the death trial).

Reproduction (from the worktree with `einsteinbot.sqlite3` present and the P6 rebuild cache in `CACHE`): deterministic slice replay `cargo test --release --lib diagnose_resumable -- --ignored --nocapture --test-threads=1` with `SPECS="sl=300:ns=6:ceil=100000:settle=1 sl=300:ns=6:ceil=100000:restart=1"`, `THREADS=2 PATHS=60 SEEDS=7101 LIVE=-` (or `SEEDS=- LIVE=35`); latency `SPECS="sl=300:ns=1:ceil=250" THREADS=8 SEEDS=- LIVE=33`; schedule `SCHEDULE=500` (or 2000) with `B="lv=0.02/0.005:cap=1000:rf=300:ev=300:lr=100:dl=30:dn=4 lv=0.02/0.005:cap=1000:rf=300:ev=1500:lr=100:dl=30:dn=4"`; B* cap 300 quality `diagnose_incremental` with `CONFIGS="lv=0.02/0.005:cap=1000:rf=300:ev=300:lr=100:dl=30:dn=4"`. Tables: `python3 -I scripts/p7_tables.py {latency,timeline} FILES`, `python3 -I scripts/p6_tables.py quality FILES --worlds syn|live`.

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

#### Stage 2: all usable captures (observed, 2026-11-27)

Launched 11:50 UTC after the pilot looked sane, finished 12:57 UTC (about 67 minutes against the 60-90 minute estimate). 29 further captures as a queue of 8 at a time at 4 threads each (B* at 2 threads), on jolly; all 32 runs (pilot 3 + stage 2 29) exited 0. Same settings as the pilot, except 4 threads for (a)-(c). Raw outputs on jolly in `/tmp/P8/out` (`pilot_*`, `s2_*`), the cascade cache in `/tmp/P8/cache`; the parsed rows are in `docs/p8/p8_results.csv`.

Captures. 32 with an oracle map and at least one first-epoch path were run (7, 8, 10, 11, 13, 14, 15, 16, 17, 21, 22, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 39, 40, 41, 42, 43, 45, 47, 48). Of them 23 have 37-61 paths; 9 have too little data to expect a map (34: 2 paths, 21: 3, 10: 4, 11, 13 and 36: 5, 26: 12, 7: 18, 27: 28) and are shown separately below. Skipped:
- Reserved (not run, not inspected): captures 12, 18, 44, 49, 52. I read "scorecard cases 12/18/44/49" as capture ids as well; 12 and 18 would have had no first-epoch path anyway.
- No `planets` message at all (no oracle): 1, 2, 3, 4, 5, 6, 19.
- Oracle but no completed path in the first map epoch (nothing to fit): 9, 20, 23, 24, 38, 46, 50, 51.
(This survey counted distinct `shot_fin` events per capture in SQL, before the runs; the harness' own path counts are in the table.)

Per-capture rows (K, recall(2) and recall(15) of the true planets, spurious = fitted planets with no true planet within 15 units, chi2 ratio = exact chi2/row over the true map's, field errors relative to the flat map on the 20-unit grid with explored = within 30 units of a path row, evals and wall seconds for prod and the cascade; B* is the maintained map at the last path, `+x` = after one guarded exact refit; flat's chi2 ratio, 1e11-1e13, is kept for reference). One run per cell:

| capture | paths | method | K | rec2 | rec15 | spur | chi2 ratio | field expl/flat | unexpl/flat | whole/flat | evals | wall s |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| 7 | 18 | flat | 0 | 0/24 | 0/24 | 0 | 3.44e+12 | 1 | 1 | 1 |  |  |
| 7 | 18 | prod | 15 | 0/24 | 2/24 | 13 | 9.01e+10 | 1.86 | 16.9 | 14.7 | 36278 | 84.6 |
| 7 | 18 | cascade | 22 | 20/24 | 21/24 | 1 | 3.94e+05 | 4.82e-03 | 2.67 | 2.31 | 36511 | 66.6 |
| 7 | 18 | B* | 23 | 18/24 | 21/24 | 2 | 3.65e+06 | 3.58e-03 | 14.4 | 12.4 |  |  |
| 7 | 18 | B*+x | 23 | 18/24 | 21/24 | 2 | 3.65e+06 | 3.58e-03 | 14.4 | 12.4 |  |  |
| 8 | 37 | flat | 0 | 0/24 | 0/24 | 0 | 5.06e+13 | 1 | 1 | 1 |  |  |
| 8 | 37 | prod | 23 | 8/24 | 12/24 | 11 | 5.94e+08 | 0.374 | 10.6 | 8.85 | 74655 | 261.1 |
| 8 | 37 | cascade | 22 | 15/24 | 18/24 | 4 | 9.10e+05 | 7.65e-04 | 15.9 | 13.3 | 34707 | 122.3 |
| 8 | 37 | B* | 20 | 14/24 | 17/24 | 3 | 2.18e+08 | 3.51e-03 | 17.3 | 14.5 |  |  |
| 8 | 37 | B*+x | 20 | 14/24 | 17/24 | 3 | 2.18e+08 | 3.51e-03 | 17.3 | 14.5 |  |  |
| 10 | 4 | flat | 0 | 0/24 | 0/24 | 0 | 5.84e+11 | 1 | 1 | 1 |  |  |
| 10 | 4 | prod | 7 | 0/24 | 0/24 | 7 | 2.85e+11 | 4.86 | 6.56 | 6.43 | 8028 | 0.4 |
| 10 | 4 | cascade | 18 | 11/24 | 14/24 | 4 | 7.16e+05 | 9.73e-03 | 65.8 | 62.9 | 36437 | 14.8 |
| 10 | 4 | B* | 13 | 1/24 | 5/24 | 8 | 8.66e+09 | 0.208 | 2.71 | 2.59 |  |  |
| 10 | 4 | B*+x | 13 | 1/24 | 5/24 | 8 | 8.66e+09 | 0.208 | 2.71 | 2.59 |  |  |
| 11 | 5 | flat | 0 | 0/24 | 0/24 | 0 | 9.80e+11 | 1 | 1 | 1 |  |  |
| 11 | 5 | prod | 7 | 0/24 | 0/24 | 7 | 2.48e+11 | 1.17 | 7.06 | 6.75 | 8615 | 1.3 |
| 11 | 5 | cascade | 19 | 8/24 | 15/24 | 4 | 3.50e+06 | 0.0806 | 3.6 | 3.44 | 36850 | 19.9 |
| 11 | 5 | B* | 12 | 0/24 | 6/24 | 6 | 9.47e+09 | 0.589 | 2.83 | 2.71 |  |  |
| 11 | 5 | B*+x | 12 | 0/24 | 6/24 | 6 | 9.47e+09 | 0.589 | 2.83 | 2.71 |  |  |
| 13 | 5 | flat | 0 | 0/24 | 0/24 | 0 | 8.03e+11 | 1 | 1 | 1 |  |  |
| 13 | 5 | prod | 8 | 0/24 | 2/24 | 6 | 1.50e+09 | 0.392 | 16.8 | 16.2 | 9934 | 1.3 |
| 13 | 5 | cascade | 16 | 6/24 | 10/24 | 6 | 2.33e+05 | 0.0308 | 7.05 | 6.78 | 36578 | 18.4 |
| 13 | 5 | B* | 11 | 1/24 | 6/24 | 5 | 3.18e+07 | 0.102 | 62.8 | 60.4 |  |  |
| 13 | 5 | B*+x | 11 | 1/24 | 6/24 | 5 | 3.18e+07 | 0.102 | 62.8 | 60.4 |  |  |
| 14 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 2.62e+12 | 1 | 1 | 1 |  |  |
| 14 | 61 | prod | 23 | 20/24 | 22/24 | 1 | 290 | 3.42e-03 | 10 | 7.65 | 119904 | 1135.2 |
| 14 | 61 | cascade | 23 | 20/24 | 22/24 | 1 | 2.56e+03 | 9.43e-04 | 2.89 | 2.21 | 40120 | 394.2 |
| 14 | 61 | B* | 22 | 18/24 | 19/24 | 3 | 2.67e+06 | 1.59e-03 | 5.2 | 3.97 |  |  |
| 14 | 61 | B*+x | 22 | 18/24 | 19/24 | 3 | 2.67e+06 | 1.59e-03 | 5.2 | 3.97 |  |  |
| 15 | 43 | flat | 0 | 0/24 | 0/24 | 0 | 5.83e+12 | 1 | 1 | 1 |  |  |
| 15 | 43 | prod | 24 | 10/24 | 17/24 | 7 | 1.88e+09 | 0.308 | 3.41 | 2.08 | 85707 | 586.8 |
| 15 | 43 | cascade | 24 | 24/24 | 24/24 | 0 | 0.995 | 8.28e-06 | 1.24e-05 | 9.99e-06 | 32458 | 65.4 |
| 15 | 43 | B* | 24 | 23/24 | 24/24 | 0 | 5.16e+07 | 1.13e-03 | 0.0349 | 0.0212 |  |  |
| 15 | 43 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.995 | 8.28e-06 | 1.24e-05 | 9.99e-06 |  |  |
| 16 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 2.68e+12 | 1 | 1 | 1 |  |  |
| 16 | 61 | prod | 24 | 1/24 | 8/24 | 16 | 5.92e+09 | 0.719 | 20.4 | 14.8 | 122755 | 1088.9 |
| 16 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.996 | 1.20e-05 | 6.52e-05 | 4.81e-05 | 31095 | 89.7 |
| 16 | 61 | B* | 24 | 24/24 | 24/24 | 0 | 1.21e+04 | 2.52e-04 | 0.0122 | 8.90e-03 |  |  |
| 16 | 61 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.996 | 1.20e-05 | 6.52e-05 | 4.81e-05 |  |  |
| 17 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 2.88e+12 | 1 | 1 | 1 |  |  |
| 17 | 61 | prod | 19 | 4/24 | 12/24 | 6 | 5.69e+09 | 0.898 | 9.75 | 6.98 | 121747 | 902.4 |
| 17 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.995 | 1.58e-05 | 2.82e-05 | 2.30e-05 | 40698 | 123.5 |
| 17 | 61 | B* | 24 | 24/24 | 24/24 | 0 | 6.38e+04 | 2.01e-04 | 6.77e-03 | 4.83e-03 |  |  |
| 17 | 61 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.995 | 1.58e-05 | 2.82e-05 | 2.30e-05 |  |  |
| 21 | 3 | flat | 0 | 0/24 | 0/24 | 0 | 2.15e+12 | 1 | 1 | 1 |  |  |
| 21 | 3 | prod | 7 | 1/24 | 2/24 | 5 | 6.54e+09 | 0.608 | 4.1 | 4.02 | 6070 | 0.2 |
| 21 | 3 | cascade | 13 | 4/24 | 5/24 | 8 | 1.69e+05 | 4.71e-03 | 65.7 | 64.5 | 36522 | 9.0 |
| 21 | 3 | B* | 9 | 2/24 | 3/24 | 6 | 8.57e+09 | 0.157 | 4.74 | 4.65 |  |  |
| 21 | 3 | B*+x | 9 | 2/24 | 3/24 | 6 | 8.57e+09 | 0.157 | 4.74 | 4.65 |  |  |
| 22 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 2.08e+12 | 1 | 1 | 1 |  |  |
| 22 | 61 | prod | 24 | 0/24 | 8/24 | 16 | 3.28e+10 | 15.1 | 24.2 | 19.2 | 121301 | 1409.4 |
| 22 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.992 | 9.38e-06 | 1.92e-03 | 1.21e-03 | 38526 | 433.6 |
| 22 | 61 | B* | 22 | 19/24 | 20/24 | 2 | 8.88e+09 | 3.01e-03 | 4.29 | 2.71 |  |  |
| 22 | 61 | B*+x | 22 | 19/24 | 20/24 | 2 | 8.88e+09 | 3.01e-03 | 4.29 | 2.71 |  |  |
| 25 | 42 | flat | 0 | 0/24 | 0/24 | 0 | 3.95e+13 | 1 | 1 | 1 |  |  |
| 25 | 42 | prod | 21 | 0/24 | 3/24 | 18 | 5.84e+11 | 4.52 | 15.7 | 10.8 | 83355 | 563.1 |
| 25 | 42 | cascade | 24 | 24/24 | 24/24 | 0 | 0.994 | 1.71e-05 | 4.90e-05 | 3.45e-05 | 27566 | 35.8 |
| 25 | 42 | B* | 24 | 24/24 | 24/24 | 0 | 2.24e+03 | 1.54e-04 | 1.65e-03 | 1.08e-03 |  |  |
| 25 | 42 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.994 | 1.71e-05 | 4.90e-05 | 3.45e-05 |  |  |
| 26 | 12 | flat | 0 | 0/24 | 0/24 | 0 | 7.91e+12 | 1 | 1 | 1 |  |  |
| 26 | 12 | prod | 13 | 0/24 | 4/24 | 9 | 1.68e+09 | 0.534 | 20.9 | 19 | 21114 | 5.3 |
| 26 | 12 | cascade | 20 | 8/24 | 15/24 | 5 | 2.00e+08 | 0.175 | 1.9 | 1.73 | 36794 | 19.9 |
| 26 | 12 | B* | 21 | 12/24 | 17/24 | 4 | 1.70e+08 | 0.0427 | 2.52 | 2.29 |  |  |
| 26 | 12 | B*+x | 21 | 12/24 | 17/24 | 4 | 1.70e+08 | 0.0427 | 2.52 | 2.29 |  |  |
| 27 | 28 | flat | 0 | 0/24 | 0/24 | 0 | 5.19e+12 | 1 | 1 | 1 |  |  |
| 27 | 28 | prod | 19 | 0/24 | 3/24 | 16 | 4.69e+11 | 16.2 | 6.24 | 10.7 | 55126 | 283.9 |
| 27 | 28 | cascade | 23 | 19/24 | 21/24 | 2 | 2.42e+04 | 3.92e-04 | 23.8 | 19.3 | 35507 | 170.5 |
| 27 | 28 | B* | 22 | 19/24 | 19/24 | 3 | 3.23e+07 | 2.01e-03 | 5.1 | 4.13 |  |  |
| 27 | 28 | B*+x | 22 | 19/24 | 19/24 | 3 | 3.23e+07 | 2.01e-03 | 5.1 | 4.13 |  |  |
| 28 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 6.61e+12 | 1 | 1 | 1 |  |  |
| 28 | 61 | prod | 24 | 24/24 | 24/24 | 0 | 21.4 | 4.60e-05 | 2.03e-04 | 1.45e-04 | 112661 | 1182.5 |
| 28 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.994 | 5.90e-06 | 1.18e-05 | 9.25e-06 | 27152 | 71.9 |
| 28 | 61 | B* | 24 | 24/24 | 24/24 | 0 | 1.87e+04 | 1.09e-04 | 2.42e-03 | 1.68e-03 |  |  |
| 28 | 61 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.994 | 5.90e-06 | 1.18e-05 | 9.25e-06 |  |  |
| 29 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 1.12e+13 | 1 | 1 | 1 |  |  |
| 29 | 61 | prod | 24 | 12/24 | 18/24 | 6 | 5.86e+07 | 0.383 | 3.32 | 2.28 | 122398 | 1487.6 |
| 29 | 61 | cascade | 25 | 16/24 | 20/24 | 5 | 1.26e+09 | 0.0208 | 41 | 27.9 | 41096 | 303.9 |
| 29 | 61 | B* | 23 | 19/24 | 19/24 | 3 | 8.21e+09 | 2.73e-03 | 4.86 | 3.31 |  |  |
| 29 | 61 | B*+x | 23 | 19/24 | 19/24 | 3 | 8.21e+09 | 2.73e-03 | 4.86 | 3.31 |  |  |
| 30 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 2.55e+13 | 1 | 1 | 1 |  |  |
| 30 | 61 | prod | 15 | 0/24 | 6/24 | 8 | 1.72e+10 | 0.497 | 16.4 | 11.4 | 122417 | 1252.3 |
| 30 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.992 | 5.59e-06 | 3.65e-05 | 2.56e-05 | 28735 | 146.1 |
| 30 | 61 | B* | 25 | 18/24 | 22/24 | 1 | 2.57e+09 | 5.95e-03 | 0.216 | 0.15 |  |  |
| 30 | 61 | B*+x | 25 | 18/24 | 22/24 | 1 | 2.57e+09 | 5.95e-03 | 0.216 | 0.15 |  |  |
| 31 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 3.05e+12 | 1 | 1 | 1 |  |  |
| 31 | 61 | prod | 23 | 1/24 | 7/24 | 15 | 1.47e+10 | 1.33 | 743 | 538 | 114792 | 1305.6 |
| 31 | 61 | cascade | 22 | 19/24 | 19/24 | 3 | 1.59e+05 | 7.76e-04 | 11 | 7.93 | 23249 | 58.6 |
| 31 | 61 | B* | 22 | 19/24 | 19/24 | 2 | 7.66e+09 | 3.42e-03 | 27.3 | 19.8 |  |  |
| 31 | 61 | B*+x | 22 | 19/24 | 19/24 | 2 | 7.66e+09 | 3.42e-03 | 27.3 | 19.8 |  |  |
| 32 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 6.87e+12 | 1 | 1 | 1 |  |  |
| 32 | 61 | prod | 21 | 0/24 | 4/24 | 17 | 5.94e+10 | 2.79 | 7.29 | 5.5 | 118555 | 1605.2 |
| 32 | 61 | cascade | 23 | 21/24 | 23/24 | 0 | 1.90e+05 | 2.31e-04 | 0.209 | 0.147 | 29665 | 93.8 |
| 32 | 61 | B* | 23 | 22/24 | 22/24 | 1 | 1.00e+07 | 5.36e-04 | 0.477 | 0.336 |  |  |
| 32 | 61 | B*+x | 23 | 22/24 | 22/24 | 1 | 1.00e+07 | 5.36e-04 | 0.477 | 0.336 |  |  |
| 33 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 9.83e+11 | 1 | 1 | 1 |  |  |
| 33 | 61 | prod | 24 | 2/24 | 7/24 | 17 | 5.02e+10 | 2.75 | 29.2 | 18.9 | 122609 | 1273.8 |
| 33 | 61 | cascade | 26 | 18/24 | 20/24 | 6 | 3.75e+07 | 0.17 | 1.28 | 0.834 | 41162 | 118.4 |
| 33 | 61 | B* | 23 | 22/24 | 22/24 | 1 | 4.67e+07 | 1.62e-03 | 2.94 | 1.9 |  |  |
| 33 | 61 | B*+x | 23 | 22/24 | 22/24 | 1 | 4.67e+07 | 1.62e-03 | 2.94 | 1.9 |  |  |
| 34 | 2 | flat | 0 | 0/24 | 0/24 | 0 | 1.93e+11 | 1 | 1 | 1 |  |  |
| 34 | 2 | prod | 3 | 0/24 | 0/24 | 3 | 7.75e+09 | 1.33 | 3.68 | 3.57 | 2227 | 0.0 |
| 34 | 2 | cascade | 18 | 2/24 | 7/24 | 11 | 1.51 | 0.0328 | 10.5 | 10.2 | 28437 | 6.8 |
| 34 | 2 | B* | 5 | 0/24 | 2/24 | 3 | 2.97e+09 | 0.454 | 2.88 | 2.79 |  |  |
| 34 | 2 | B*+x | 5 | 0/24 | 2/24 | 3 | 2.97e+09 | 0.454 | 2.88 | 2.79 |  |  |
| 35 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 7.44e+12 | 1 | 1 | 1 |  |  |
| 35 | 61 | prod | 24 | 12/24 | 15/24 | 9 | 4.70e+08 | 0.661 | 4.16 | 2.79 | 117013 | 1362.5 |
| 35 | 61 | cascade | 24 | 24/24 | 24/24 | 0 | 0.995 | 5.53e-06 | 3.54e-05 | 2.37e-05 | 28834 | 69.2 |
| 35 | 61 | B* | 24 | 24/24 | 24/24 | 0 | 3.02e+05 | 2.02e-04 | 5.36e-03 | 3.54e-03 |  |  |
| 35 | 61 | B*+x | 24 | 24/24 | 24/24 | 0 | 0.995 | 5.53e-06 | 3.54e-05 | 2.37e-05 |  |  |
| 36 | 5 | flat | 0 | 0/24 | 0/24 | 0 | 8.72e+10 | 1 | 1 | 1 |  |  |
| 36 | 5 | prod | 8 | 0/24 | 0/24 | 8 | 5.78e+10 | 2.25 | 13 | 12.4 | 10184 | 0.9 |
| 36 | 5 | cascade | 15 | 3/24 | 11/24 | 4 | 1.37e+08 | 0.117 | 2.55 | 2.42 | 36619 | 7.1 |
| 36 | 5 | B* | 13 | 1/24 | 6/24 | 7 | 5.17e+08 | 0.263 | 3.53 | 3.35 |  |  |
| 36 | 5 | B*+x | 13 | 1/24 | 6/24 | 7 | 5.17e+08 | 0.263 | 3.53 | 3.35 |  |  |
| 37 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 8.89e+11 | 1 | 1 | 1 |  |  |
| 37 | 61 | prod | 24 | 21/24 | 22/24 | 1 | 6.07e+04 | 0.0181 | 1.27 | 0.935 | 118594 | 893.6 |
| 37 | 61 | cascade | 25 | 21/24 | 22/24 | 3 | 1.33e+06 | 5.81e-03 | 2.17 | 1.6 | 40943 | 329.5 |
| 37 | 61 | B* | 25 | 22/24 | 24/24 | 0 | 4.48e+04 | 1.64e-04 | 0.0816 | 0.0602 |  |  |
| 37 | 61 | B*+x | 25 | 22/24 | 24/24 | 0 | 0.992 | 3.21e-05 | 0.0811 | 0.0599 |  |  |
| 39 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 4.93e+12 | 1 | 1 | 1 |  |  |
| 39 | 61 | prod | 18 | 1/24 | 7/24 | 11 | 1.90e+10 | 12.1 | 5.97 | 9.92 | 122037 | 1693.3 |
| 39 | 61 | cascade | 23 | 22/24 | 22/24 | 1 | 3.19e+04 | 9.92e-04 | 3.62 | 2.38 | 27336 | 116.9 |
| 39 | 61 | B* | 23 | 21/24 | 22/24 | 1 | 3.01e+07 | 7.14e-04 | 4.71 | 3.09 |  |  |
| 39 | 61 | B*+x | 23 | 21/24 | 22/24 | 1 | 3.01e+07 | 7.14e-04 | 4.71 | 3.09 |  |  |
| 40 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 1.42e+12 | 1 | 1 | 1 |  |  |
| 40 | 61 | prod | 22 | 19/24 | 21/24 | 1 | 3.70e+03 | 7.96e-03 | 2.42 | 1.76 | 120366 | 698.9 |
| 40 | 61 | cascade | 23 | 20/24 | 22/24 | 1 | 459 | 1.18e-04 | 1.26 | 0.913 | 28456 | 95.4 |
| 40 | 61 | B* | 22 | 19/24 | 21/24 | 1 | 2.06e+06 | 2.53e-03 | 34.7 | 25.2 |  |  |
| 40 | 61 | B*+x | 22 | 19/24 | 21/24 | 1 | 2.06e+06 | 2.53e-03 | 34.7 | 25.2 |  |  |
| 41 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 9.81e+12 | 1 | 1 | 1 |  |  |
| 41 | 61 | prod | 23 | 0/24 | 2/24 | 19 | 8.58e+11 | 25.8 | 10.6 | 20.2 | 120719 | 1929.8 |
| 41 | 61 | cascade | 22 | 20/24 | 20/24 | 2 | 1.69e+08 | 0.105 | 134 | 90.9 | 44796 | 304.1 |
| 41 | 61 | B* | 23 | 22/24 | 22/24 | 1 | 6.63e+07 | 1.57e-03 | 111 | 75.4 |  |  |
| 41 | 61 | B*+x | 23 | 22/24 | 22/24 | 1 | 6.63e+07 | 1.57e-03 | 111 | 75.4 |  |  |
| 42 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 1.32e+13 | 1 | 1 | 1 |  |  |
| 42 | 61 | prod | 23 | 15/24 | 20/24 | 3 | 5.87e+07 | 0.137 | 0.761 | 0.531 | 122696 | 413.9 |
| 42 | 61 | cascade | 23 | 21/24 | 21/24 | 2 | 1.65e+07 | 1.58e-03 | 0.329 | 0.226 | 38664 | 375.8 |
| 42 | 61 | B* | 22 | 20/24 | 21/24 | 1 | 2.64e+07 | 1.56e-03 | 0.324 | 0.222 |  |  |
| 42 | 61 | B*+x | 22 | 20/24 | 21/24 | 1 | 2.64e+07 | 1.56e-03 | 0.324 | 0.222 |  |  |
| 43 | 41 | flat | 0 | 0/24 | 0/24 | 0 | 1.25e+14 | 1 | 1 | 1 |  |  |
| 43 | 41 | prod | 23 | 0/24 | 0/24 | 23 | 7.13e+12 | 83.9 | 27 | 62 | 82122 | 696.0 |
| 43 | 41 | cascade | 24 | 24/24 | 24/24 | 0 | 0.993 | 8.31e-06 | 3.72e-06 | 6.41e-06 | 28786 | 37.7 |
| 43 | 41 | B* | 25 | 24/24 | 24/24 | 0 | 4.02e+04 | 7.74e-05 | 1.70e-04 | 1.32e-04 |  |  |
| 43 | 41 | B*+x | 25 | 24/24 | 24/24 | 0 | 0.993 | 7.99e-06 | 3.67e-06 | 6.19e-06 |  |  |
| 45 | 61 | flat | 0 | 0/24 | 0/24 | 0 | 8.33e+11 | 1 | 1 | 1 |  |  |
| 45 | 61 | prod | 22 | 14/24 | 18/24 | 3 | 1.44e+07 | 0.0353 | 5.51 | 4 | 111391 | 459.2 |
| 45 | 61 | cascade | 22 | 16/24 | 19/24 | 2 | 1.48e+07 | 0.199 | 2.48 | 1.81 | 40519 | 71.6 |
| 45 | 61 | B* | 22 | 21/24 | 21/24 | 1 | 8.52e+06 | 2.40e-03 | 3.61 | 2.62 |  |  |
| 45 | 61 | B*+x | 22 | 21/24 | 21/24 | 1 | 8.52e+06 | 2.40e-03 | 3.61 | 2.62 |  |  |
| 47 | 38 | flat | 0 | 0/24 | 0/24 | 0 | 4.63e+12 | 1 | 1 | 1 |  |  |
| 47 | 38 | prod | 22 | 19/24 | 20/24 | 2 | 1.29e+04 | 7.52e-03 | 2.28 | 1.85 | 74382 | 253.4 |
| 47 | 38 | cascade | 23 | 19/24 | 20/24 | 3 | 1.52e+04 | 2.32e-03 | 1.78 | 1.44 | 36208 | 109.1 |
| 47 | 38 | B* | 22 | 19/24 | 20/24 | 2 | 1.66e+07 | 6.52e-03 | 1.79 | 1.45 |  |  |
| 47 | 38 | B*+x | 22 | 19/24 | 20/24 | 2 | 1.66e+07 | 6.52e-03 | 1.79 | 1.45 |  |  |
| 48 | 48 | flat | 0 | 0/24 | 0/24 | 0 | 1.66e+13 | 1 | 1 | 1 |  |  |
| 48 | 48 | prod | 24 | 12/24 | 17/24 | 7 | 5.80e+08 | 0.127 | 9.66 | 6.91 | 94065 | 714.4 |
| 48 | 48 | cascade | 24 | 24/24 | 24/24 | 0 | 0.992 | 9.48e-06 | 5.88e-04 | 4.20e-04 | 24962 | 72.0 |
| 48 | 48 | B* | 23 | 20/24 | 22/24 | 1 | 8.89e+08 | 5.78e-04 | 3.55 | 2.54 |  |  |
| 48 | 48 | B*+x | 23 | 20/24 | 22/24 | 1 | 8.89e+08 | 5.78e-04 | 3.55 | 2.54 |  |  |

Comparison counts, all 32 captures. "better / equal / worse" is the first method against the second per metric: recall higher is better, spurious, chi2 ratio and field error lower is better; discrete metrics equal only when identical, continuous within 1%. Spurious against flat is "worse or equal" by construction (flat has none); read that column only for cascade against prod and for B*+x against the cascade.

Counts over captures (better / equal / worse of the first method against the second; n = captures where both are defined).

| comparison | recall(2) | recall(15) | spurious | chi2 ratio | field explored/flat | field unexplored/flat | field whole/flat |
|---|---|---|---|---|---|---|---|
| prod vs flat | 18 / 14 / 0 (n=32) | 27 / 5 / 0 (n=32) | 0 / 1 / 31 (n=32) | 32 / 0 / 0 (n=32) | 18 / 0 / 14 (n=32) | 2 / 0 / 30 (n=32) | 3 / 0 / 29 (n=32) |
| cascade vs flat | 32 / 0 / 0 (n=32) | 32 / 0 / 0 (n=32) | 0 / 11 / 21 (n=32) | 32 / 0 / 0 (n=32) | 32 / 0 / 0 (n=32) | 12 / 0 / 20 (n=32) | 14 / 0 / 18 (n=32) |
| cascade vs prod | 28 / 4 / 0 (n=32) | 28 / 4 / 0 (n=32) | 24 / 4 / 4 (n=32) | 27 / 0 / 5 (n=32) | 31 / 0 / 1 (n=32) | 24 / 0 / 8 (n=32) | 24 / 0 / 8 (n=32) |
| B*+x vs flat | 30 / 2 / 0 (n=32) | 32 / 0 / 0 (n=32) | 0 / 8 / 24 (n=32) | 32 / 0 / 0 (n=32) | 32 / 0 / 0 (n=32) | 11 / 0 / 21 (n=32) | 11 / 0 / 21 (n=32) |
| B*+x vs cascade | 7 / 10 / 15 (n=32) | 5 / 12 / 15 (n=32) | 13 / 9 / 10 (n=32) | 4 / 7 / 21 (n=32) | 10 / 6 / 16 (n=32) | 10 / 7 / 15 (n=32) | 10 / 7 / 15 (n=32) |

Only the 23 captures with 37-61 paths:

Counts over captures (better / equal / worse of the first method against the second; n = captures where both are defined).

| comparison | recall(2) | recall(15) | spurious | chi2 ratio | field explored/flat | field unexplored/flat | field whole/flat |
|---|---|---|---|---|---|---|---|
| prod vs flat | 17 / 6 / 0 (n=23) | 22 / 1 / 0 (n=23) | 0 / 1 / 22 (n=23) | 23 / 0 / 0 (n=23) | 15 / 0 / 8 (n=23) | 2 / 0 / 21 (n=23) | 3 / 0 / 20 (n=23) |
| cascade vs flat | 23 / 0 / 0 (n=23) | 23 / 0 / 0 (n=23) | 0 / 11 / 12 (n=23) | 23 / 0 / 0 (n=23) | 23 / 0 / 0 (n=23) | 12 / 0 / 11 (n=23) | 14 / 0 / 9 (n=23) |
| cascade vs prod | 19 / 4 / 0 (n=23) | 19 / 4 / 0 (n=23) | 18 / 3 / 2 (n=23) | 18 / 0 / 5 (n=23) | 22 / 0 / 1 (n=23) | 19 / 0 / 4 (n=23) | 19 / 0 / 4 (n=23) |
| B*+x vs flat | 23 / 0 / 0 (n=23) | 23 / 0 / 0 (n=23) | 0 / 8 / 15 (n=23) | 23 / 0 / 0 (n=23) | 23 / 0 / 0 (n=23) | 11 / 0 / 12 (n=23) | 11 / 0 / 12 (n=23) |
| B*+x vs cascade | 6 / 9 / 8 (n=23) | 4 / 11 / 8 (n=23) | 9 / 9 / 5 (n=23) | 3 / 7 / 13 (n=23) | 8 / 6 / 9 (n=23) | 5 / 7 / 11 (n=23) | 5 / 7 / 11 (n=23) |

Only the 9 captures with 2-28 paths:

Counts over captures (better / equal / worse of the first method against the second; n = captures where both are defined).

| comparison | recall(2) | recall(15) | spurious | chi2 ratio | field explored/flat | field unexplored/flat | field whole/flat |
|---|---|---|---|---|---|---|---|
| prod vs flat | 1 / 8 / 0 (n=9) | 5 / 4 / 0 (n=9) | 0 / 0 / 9 (n=9) | 9 / 0 / 0 (n=9) | 3 / 0 / 6 (n=9) | 0 / 0 / 9 (n=9) | 0 / 0 / 9 (n=9) |
| cascade vs flat | 9 / 0 / 0 (n=9) | 9 / 0 / 0 (n=9) | 0 / 0 / 9 (n=9) | 9 / 0 / 0 (n=9) | 9 / 0 / 0 (n=9) | 0 / 0 / 9 (n=9) | 0 / 0 / 9 (n=9) |
| cascade vs prod | 9 / 0 / 0 (n=9) | 9 / 0 / 0 (n=9) | 6 / 1 / 2 (n=9) | 9 / 0 / 0 (n=9) | 9 / 0 / 0 (n=9) | 5 / 0 / 4 (n=9) | 5 / 0 / 4 (n=9) |
| B*+x vs flat | 7 / 2 / 0 (n=9) | 9 / 0 / 0 (n=9) | 0 / 0 / 9 (n=9) | 9 / 0 / 0 (n=9) | 9 / 0 / 0 (n=9) | 0 / 0 / 9 (n=9) | 0 / 0 / 9 (n=9) |
| B*+x vs cascade | 1 / 1 / 7 (n=9) | 1 / 1 / 7 (n=9) | 4 / 0 / 5 (n=9) | 1 / 0 / 8 (n=9) | 2 / 0 / 7 (n=9) | 5 / 0 / 4 (n=9) | 5 / 0 / 4 (n=9) |

Captures where a method is worse than the one it is compared with, on any of recall(2), recall(15), chi2 ratio, explored, unexplored or whole field (ex spurious): cascade against flat 20 of 32 (all 9 small ones plus 8, 14, 29, 31, 33, 37, 39, 40, 41, 45, 47); production against flat 30 of 32; cascade against production 11 of 32 (8, 10, 14, 21, 27, 29, 34, 37, 41, 45, 47). The by-metric lists come from `python3 -I scripts/p8_tables.py summary ...`.

#### What the numbers say (observed unless marked)

- The user's ideal, "the new implementation is better in all cases", holds for the from-scratch cascade only in the explored area and in recall: against flat it has higher recall(2) and recall(15) in 32 of 32 captures, lower chi2 ratio in 32 of 32 and lower explored-area field error in 32 of 32 (explored error 1e-6 to 0.2 of flat). It does not hold for the unexplored area and the whole field: unexplored error is above flat in 20 of 32 captures (whole field above flat in 18), and the cascade has spurious planets in 21 of 32 (flat has none, by definition). Against the production search the cascade has the higher recall(2) in 28, equal in 4 (14, 28, 37, 47), never lower; it is worse than production on the explored field in 1 capture (45), on the unexplored field in 8, on the chi2 ratio in 5, on spurious count in 4.
- Production (default `Estimator`, 2000 evaluations per update, about 120k per capture at 61 paths) is much worse than the cascade and often worse than flat: mean recall(2) 6.1 of 24 against 17.8 (cascade), on the 23 larger captures 8.5 against 21.2; it is worse than flat in the explored area in 14 of 32 captures (e.g. 41: 26x flat, 43: 84x flat, with 19-23 spurious planets) and worse in the unexplored area in 30 of 32. It reaches 24/24 on one capture (28) only. Every update used the evaluation limit; the slowest single update took 82 s at 4 threads, so 2000 is far above what the live budget (5 s, about 250-440 evaluations at K = 24 in P6) allows; production at its real live budget would be worse, not better (inferred).
- Cascade (from scratch, on all paths, 23-45k raw evaluations, 7-433 s at 4 threads): complete map (24/24) and chi2 ratio 0.99 on 10 of the 23 larger captures (15, 16, 17, 22, 25, 28, 30, 35, 43, 48; 15 and 25 and 43 have 41-43 paths); with ratio above 1.2 elsewhere (live truth is degenerate and an incomplete map is punished by the exact likelihood). Mean recall(2) 21.2 of 24 on the larger captures.
- B* (incremental, last path): mean recall(2) 21.0 on the larger captures against 21.2 for the cascade; complete on 6 (7 after the guarded exact refit, ratio <= 1.2 on 8 after it). It is equal or better than the cascade in recall(2) in 17 of 32 captures (7 better, 10 equal) and worse in 15: better on 33 (22 against 18), 41 (22 against 20), 45 (21 against 16), 29 (19 against 16) and 37 (22 against 21), worse on 22 (19 against 24), 30 (18 against 24), 48 (20 against 24), 14 (18 against 20) and on most small captures (e.g. 10, 11, 13: 0-1 against 6-11 of 24). That matches P6 (B behind the cascade at few paths, at least equal from about 16 paths on the synthetic worlds; live results mixed).

Where the new implementation is NOT better than flat (cascade; the reasons are partly inferred):
- Spurious planets exist at all: flat has none. Count against flat is a property of the metric, not a regression in the field.
- Unexplored-area and whole-field error above flat (20 captures in unexplored: 7, 8, 10, 11, 13, 14, 21, 26, 27, 29, 31, 33, 34, 36, 37, 39, 40, 41, 45, 47). Two causes seen in the data: (i) small data: all nine captures with at most 28 paths (the maps are 3-19 of 24 planets) are worse than flat there, as in P6's synthetic data at 4-16 paths; (ii) on the 23 larger captures, every one of the 11 that is worse than flat somewhere (8, 14, 29, 31, 33, 37, 39, 40, 41, 45, 47) has an incomplete map: recall(2) 15-22 of 24, and 1-6 spurious planets (a misplaced planet adds a false pull in unexplored space, e.g. capture 41: two fitted planets 46 and 59 units from any true planet, error 134x flat in the unexplored area; capture 29: five spurious planets, two of them 399 and 557 units from any true planet, 41x flat; capture 31: 11x flat), and 1-4 true planets with no fitted planet within 44-200 units (checked against the cached maps; that these sit in parts of the field the paths hardly visit is inferred, not measured). The whole-field error is still below flat on 33 and 40 (0.83, 0.91) although their unexplored error is 1.3x flat. The 10 complete maps have unexplored error 4e-6 to 2e-3 of flat.
- The explored area itself is never worse than flat for the cascade; B*+x is also never worse than flat there (32 of 32), production is (14 of 32).

Hypothesis, not tested here: publishing a full map is only worth it past the point where the map is nearly complete; before that a gate (publish a planet only with support, or publish the flat field away from the data) would avoid the unexplored-area loss, but P6 showed that the removal chi2 is not a usable support statistic (Q4), so this stays open.

#### Caveats and limits

- One run per cell, no repeats; evaluation-count budgets make (b) and (c) deterministic, but the (b) result depends on the per-update limit I chose (2000); a larger or smaller limit was not tried, and the live production update has a 5 s wall budget instead. Production here is fed without contacts and without player positions (as in the other diagnosis harnesses), and not as the background thread of the real client; its warm starts are the estimator's own.
- The cascade was run once on all paths (from scratch), (b) incrementally path by path: they do the same work only loosely (cascade 25-45k evaluations total against about 120k for production). B* was run with the P6 settings that were chosen on dev worlds 33, 39, 40, 41, 45, 48, so its numbers on those are not independent; the cascade parameters were tuned on synthetic data and checked on live dev 33/39/40/41/45/48 in E9c, so captures 33-48 are not unseen for it either. The other 26 captures were never used for tuning (E9 held-out live captures 34, 35, 37, 42, 43 were looked at once); the cascade is better than flat in all explored-area metrics there too.
- "Equal" for continuous metrics is a 1% band. Captures are taken in recording order (spatially correlated), truth is degenerate on live data, so chi2 ratios of 1e3-1e9 for incomplete maps are not informative about how close the map is.
- Paths: at most 61 per capture (first map epoch); 9 captures have 2-28. Oracle truth only scores (also the diagnosis of misplaced planets above, which I did against the cached maps after the runs).
- The wall times are indicative (shared machine, load up to 27 from other jobs, 4 threads). Evaluation counts are exact. No run exceeded a runaway ceiling (slowest single production update 82 s).

Reproduction. Build once: `cargo test --release --lib --no-run` in the worktree; with `einsteinbot.sqlite3` in the working directory and `SEEDS=-` (no synthetic worlds) run, per capture `C`:
`LIVE=C SEEDS=- PATHS=60 THREADS=4 CACHE=/tmp/P8/cache PROD_EVALS=2000 cargo test --release --lib diagnose_baseline -- --ignored --nocapture --test-threads=1` (flat, prod, cascade; `METHODS=flat,prod,rebuild` default) and
`LIVE=C SEEDS=- PATHS=60 THREADS=2 CACHE=/tmp/P8/cache CHECKPOINTS=1000 CONFIGS=lv=0.02/0.005:cap=1000:rf=300:ev=1500:lr=100:dl=30:dn=4 cargo test --release --lib diagnose_incremental -- --ignored --nocapture --test-threads=1` (B*; `CHECKPOINTS=1000` keeps only the last path count). Tables: `python3 -I scripts/p8_tables.py {rows,summary,csv} OUTPUT_FILES... [--min-paths N] [--max-paths N]`.

### P11 results (2026-11-27, branch `p11`, built on `p7` and `p8`; harness `src/estimator/diagnose/replay.rs` (ignored test `diagnose_replay`) and `diagnose/baseline.rs` (`diagnose_baseline`); tables from `scripts/p6_tables.py quality`, `scripts/p7_tables.py timeline` and `scripts/p8_tables.py`)

Decision (user, 2026-11-27): v2 stays the estimator and the measured search improvements become the production default, replacing the greedy-birth search wholesale. The question of this step: does the production code, run through the real `Estimator` API, reproduce the P7 numbers, and what does the audit add? Truth scores only; contacts create nothing (the replayed paths carry no contacts, so the contact path is covered by unit tests only). Grades: observed unless marked.

#### What the production estimator is now

- Per path (`Estimator::observe_batch`): local refit of the planets within 100 units of the path (300 evaluations, thinned rows, relative sd 0.02 then 0.005), the map is published at once.
- Then up to 6 slices of 300 evaluations (250 ms wall ceiling each), run by `Estimator::step` (the live map's worker calls it while no event is queued): resumable births (`search.rs`, the P7 `Search`: candidates, alone fits, joint partials, death trial after each birth), state surviving across slices and paths.
- After the slices, one polish step (`rebuild::exact_refit`): the guarded exact refit (accepted only if the exact chi2 falls and no planet moves more than 3 units or chi2/row <= 1.5) of the published map, cut by the slice ceiling; the covariance comes from the exact evidence of the refit map when the refit finished in time. The polished map is published, the search state keeps its own map.
- Audit (`audit.rs`, off by default in `Settings`, on in the live map): at 8 paths, then every 16, a background thread builds a map from scratch (`rebuild::from_scratch`: the E9 cascade `500/0.1, 1000/0.02, 2000/0.005` then the E9c polish `0.005gd, 0.001gd, 0` with the guarded exact refit). When it is done a second thread compares it with the maintained map on all paths known then (both refit on all rows at relative 0.005, 1500 evaluations, Laplace evidence); only a higher evidence replaces the maintained map. Both threads use half of `Settings::threads`; neither blocks publication.
- Deleted: the greedy `search()` and its `Limits::deadline`/`parallel_evaluations`, mode retention and weights (`MapBelief::map_weight` and `alternatives` stay in the struct, always `None`/empty), the warm-start refit, the 0.1 absolute noise level and the whole multi-level noise machinery (`PathData::levels`, `new_relative`'s level list, `Fit` coarse-to-fine loop), `Estimator::{time_budget, max_evaluations, parallel_evaluations, noise_levels}`, the test-only Jacobian-radius/normal-equations/tolerance variants of `Fit`, the diagnosis harnesses of E9/E9c/P6/P7 (`thinning.rs`, `polish.rs`, `incremental.rs`, `resumable.rs`) and the greedy-search diagnoses of `diagnose.rs`. Git history is the fallback (last commit with them: `6b03eb1` plus the p8 merge).

#### Regression against the P7 numbers (observed; deterministic replay, `CEIL_MS=0`, 60 paths, final checkpoint per world)

Recall(2) of the true planets, dev worlds (synthetic 7001-7008; live 33, 39, 40, 41, 45, 48), at 16 / 32 / final paths. Raw output on jolly `/tmp/P11/out/f_dev_off`, `f_dev_audit`, `f_held`; the numbers of P7 are copied from its table.

| method | dev synthetic | dev live |
|---|---|---|
| P7 resumable ns=6 (harness, no polish in the state, no audit) | 61% / 100% / 100% | 44% / 66% / 89% |
| P11 production, no audit | 81% / 96% / 95% (spurious 0.5, K 24.0) | 46% / 73% / 83% (spurious 2.2, K 23.2) |
| P11 production + audit (adopted when better, run to completion right after the path that starts it) | 83% / 100% / 100% (spurious 0.0) | 44% / 74% / 92% (spurious 0.7, K 23.2) |

Held-out (one pass, final configuration = audit on; synthetic 7101-7108, live 34, 35, 37, 42, 43; live 34 has 2 paths and 43 has 41, so the live final row is the three worlds with 60 or 61 paths): synthetic 82% / 98% / 100% (spurious 0.0); live 47% / 80% / 92% (spurious 0.7; P7 resumable 27% / 46% / 85%, B* uncapped 92%). Per live world at the end: 35 24/24, 37 22/24, 42 20/24.

Reading: within noise of P7 or better. The audit decides the end state: without it three synthetic dev worlds stay at 19-22 of 24 planets and live 45 at 17 of 24 with 7 spurious; with it every dev synthetic world ends at 24/24 with the exact chi2 of the truth (ratio 0.99) and live 33 and 48 at 24/24. Adoption: 36 of 55 audits on the dev worlds (all 14 at 8 paths, 10 of 14 at 24, 6 of 14 at 40, 6 of 13 at 56), 29 of 47 on the held-out worlds. Recall at 16 paths is higher than P7's (81-83% against 61% synthetic) mainly because the audit at 8 paths adopts a better map and the review fixes below. The sync audit adopts at the path that starts it, which is optimistic: the audit takes 50-140 s on average per stage-one rebuild (2 threads, jolly under load 20-30; the Mac at 5 threads was not timed separately) so a map is adopted some paths later. The schedule runs on the Mac below include that (final recall in the 500 ms / 2 s schedule runs: live 33 19 / 16, live 40 16 / 19, live 45 18 / 21 of 24, inside the spread of the deterministic runs; one run per cell).

Evaluations per path (all 14 dev worlds, audit off): median 2.2k for paths 1-8 and 2.3k for 9-16 (max 2.9k, the owed slices run), 596 for 17-32, 207 for 33-61 (steady state: the slices find nothing and only the polish remains).

Bugs found by the measurements (fixed before these numbers): (1) an audit compared at the paths of its snapshot and then adopted minutes later threw away everything learned since (live 33 at 500 ms per path: recall(2) 17 -> 5 at the adoption), hence the two-stage audit (rebuild, then a comparison on the paths known when the rebuild ends) and an adoption that is a plain map swap; (2) the exact refit of the published map ran uncut and took 3 s at 34 paths (live 40; publish lag 2.6-10 s), hence the wall ceiling on it (one ceiling for the refit, none on the Jacobian of the covariance, which is skipped when the ceiling is used up). A reviewer pass found and I fixed: partials cut by the slice deadline were marked converged, new seeds were dropped from a full candidate list, a cancelled update could leave paths out of the search, the live worker did not adopt a finished audit while idle.

#### P8 comparison, pilot captures 33, 40, 45 (observed; 61 paths)

Replaces the old `prod` column (P8: the greedy search with 2000 evaluations per update; 111-123k evaluations and 8-21 minutes per capture):

| capture | old prod rec2 (spurious) | cascade (from scratch) | B* | new prod, no audit | new prod + audit |
|---|---|---|---|---|---|
| 33 | 2 (17) | 18 (6) | 22 (1) | 22 (1), 83k evaluations | 24 (0), chi2 ratio 0.996 |
| 40 | 19 (1) | 20 (1) | 19 (1) | 18 (2), 64k | 20 (1) |
| 45 | 14 (3) | 16 (2) | 21 (1) | 17 (7), 107k | 21 (1) |

Without the audit capture 40 is one planet below the old production (18 against 19; within the 1-3 planets P6 saw between configurations on live worlds), with it recall is at least equal everywhere and far above on 33 and 45. The production-run chi2 ratios and field errors are in the raw output (`f_base_*`). The no-audit 33 and audit 45 rows reproduce B* exactly (chi2 ratio 4.67e7 and 8.52e6 as in P8), which is the check that the production path equals the harness.

#### Latency (observed; Mac, 10 threads, release test binary, audit on with 5 threads, one process, nothing built during the runs; Mac load average 4-6 during the runs from other sessions, 3-4 before)

A path arrives every 500 ms or 2 s of wall clock; between arrivals the estimator takes slices as the live worker does. Publish lag = from a path's arrival to its map being published (61 paths on each live capture):

| capture | period | median | p90 | max |
|---|---|---|---|---|
| 33 | 500 ms | 143 ms | 249 | 378 |
| 33 | 2 s | 26 | 64 | 150 |
| 40 | 500 ms | 84 | 225 | 265 |
| 40 | 2 s | 23 | 40 | 64 |
| 45 | 500 ms | 130 | 238 | 264 |
| 45 | 2 s | 24 | 47 | 85 |

Worst publish lag 64-378 ms; five of six runs are at or below the 350 ms target, the 378 ms run (live 33, 500 ms) is 8% above it. The lag is the unfinished step (a slice is cut at 250 ms but a batch of refits can run a little over; the polish is cut at 250 ms plus the covariance Jacobian) plus the local refit. Before the fixes above the worst lags were 1.2-10 s. Without the audit thread live 40 at 500 ms had 269 ms (one run), with it 265-325 ms.

#### Test suite and known limitations

`cargo test --lib` takes 2.0 s on jolly for both the base commit and this one (94 tests, 8 ignored heavy harnesses); `cargo fmt --check` clean.

Limitations: early updates (under 8 paths) have no special rule and recall is low there (13% synthetic and 10% live at 8 paths without the audit; the audit at 8 paths lifts it afterwards); deaths run only after a birth, so a spurious planet stays until the next birth or the next audit; the comparison of an audit uses the Laplace evidence at the relative level (a heuristic, not calibrated); one run per cell, no repeats; live truth is degenerate in the sense of P6, so 1-3 planets of difference on live worlds are within what different configurations showed; the contact constraint is not exercised by the replay harness; the final map of a live game is the published polished map, whose covariance is absent (`not_computed`) while a path's slices are still running or when the refit used up its time.

#### Viewer-visible changes (for a later UI sync; the viewer UI itself was not touched)

`map_belief` diagnostics keep every key. Meaning changes: `map_weight` always `null`, `alternatives` always `[]`; `search_limit` is `planet_cap`, `work_budget` (search work pending, also right after every path) or `time_budget` (a slice hit its ceiling); `covariance`/`covariance_unavailable_reason`: a belief published right after a path says `not_computed`, the covariance arrives with the polished publication that follows. `update_report` parts (scorecard JSON): `warm_start` removed, `polish` added, `warm_refit` is the local refit of a new path. Several beliefs can now be published per path (local refit, slice changes, polish, audit adoption); captures still record the belief at decisions. `web/map_viewer.js` still reads the removed `fit_converged`, `approximate_covariance` and `field_evidence` (not fixed). `field_scorecard` flags removed: `--time-budget-ms`, `--max-evaluations`, `--serial-evaluations`, `--noise-sd`; added: `--slice-evaluations`, `--slice-ceiling-ms`, `--slices-per-path`, `--threads`, `--audit`.

Reproduction (worktree with `einsteinbot.sqlite3` in the working directory): deterministic replay `cargo test --release --lib diagnose_replay -- --ignored --nocapture --test-threads=1` with `SEEDS=7001 LIVE=- PATHS=60 THREADS=2 CEIL_MS=0 AUDIT=0 NAME=prod` (or `SEEDS=- LIVE=33`; `AUDIT=1` runs the audit to completion after the path that starts it); schedule `SCHEDULE=500` (or 2000) with `AUDIT=1` and the default threads; baseline `diagnose_baseline` with `METHODS=flat,prod,rebuild`. Tables: `python3 -I scripts/p6_tables.py quality FILES --worlds synthetic|live`, `python3 -I scripts/p7_tables.py timeline FILES`, `python3 -I scripts/p8_tables.py ...`. The harnesses of P6-P7 that the older sections cite were removed with this change; their last version is in git at `6b03eb1`.
