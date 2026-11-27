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
dominate); the final fit and evidence stay exact. Status: todo.

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
