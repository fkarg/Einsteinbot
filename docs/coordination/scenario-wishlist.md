# Scenario wishlist (viewer session → estimator session)

Draft P7, 2026-11-21, from the viewer session. Responds to contract open request 2 (`viewer-estimator-contract.md`, §5). The estimator session owns the scenario runner and metrics (`src/bin/learning_scenarios.rs`, `capture_learning_scenarios.rs`), so this is a request and a spec, not a patch. Status: requested families and sweeps remain proposed; additive metadata for the existing hand-authored scenarios has been implemented, but does not satisfy the scale/seeding wishlist.

User intent (2026-11-21): "we build more test scenarios… learn/explore where the biggest errors happen." Scenarios exist to make a *named kind of error* reproducible, comparable across revisions, and visible in the viewer.

## 1. What the evidence says the corpus is missing

Observed in the viewer's captures table (17 synthetic scenarios, 44 live captures):

- Every synthetic scenario with at least one contact converges exactly: 0 missing planets and 0.0–0.1 units median center error for 10 of 17. The rest are 0-contact flybys (100% missing by construction) or tiny partial maps (`two_planets_one_known` 50%, `three_planets_partial_swerve` 33%).
- The corpus has at most 4 planets. Live capture 44 has 24 disclosed planets and ends with 6 missing, 5 mislocated or mis-sized, and 13 close.
- Concrete live error cases (capture 44, final belief): a match 41.4 units off (radius +1.9, mass ×1.15); a match 19.8 off with radius +10.1 and mass ×2.39; planets 10–12 off; six never-seen planets (radius 20–32) spread across the map; 12 ambiguous contact associations; an overlapping pair near (580, 720) fitted as two disks of different size.
- Fit covariance is null in every live decision, so no scenario currently exercises uncertainty at all.

So the corpus tests "can it fit a clean, isolated planet" and little else. The live errors come from ambiguity, scale and partial evidence.

## 2. Requested scenario families

Each family isolates one defect class. `defect_class` is the label the viewer will group and filter by (see §3). Parameters are swept so a family is a small ladder, not one point.

| ID | `defect_class` | What it isolates | Truth / observations | Sweep | Pass criteria (suggested) |
| --- | --- | --- | --- | --- | --- |
| W1 | `extra_unseen` | One far planet nobody has touched bends paths of an otherwise established map | 2–3 established planets, one extra planet 150–600 units from any contact; paths pass it without contact | distance, mass (radius 20–40) | field-only hypothesis appears and is *not* promoted to a collision disk; held-out path RMSE improves vs no-source; no false source on W6 nulls |
| W2 | `displaced_planet` | A known planet seeded at the wrong position | Belief starts with the planet moved by d; later paths constrain it | d = 5, 10, 20, 40 units | joint fit moves it (center error ↓) rather than adding a second source; contact/free-space constraints respected |
| W3 | `wrong_radius` | Radius/mass error from one-sided contact evidence | Single-contact, then repeated same-side contacts, then multi-direction contacts | radius ±5, ±10; 1 vs 3 vs 6 contacts; one vs several approach directions | radius/mass error shrinks with approach diversity; covariance (if available) shrinks too |
| W4 | `overlapping_pair` | Two close planets merged into one disk or mis-split (capture 44 near (580, 720)) | Two planets 40–120 units apart, contacts on both | separation, radius ratio | one disk per planet within tolerance; or the ambiguity is reported (`uncertain_associations > 0`), never silently merged |
| W5 | `ambiguous_contact` | Contact association ambiguity (live: 12) | Endpoint near the surface of two candidate planets or near a player | endpoint distance to each surface, player distance | ambiguous endpoints stay explicit and do not double-count gravity |
| W6 | `null_flat` | False-source rate | Flat world and quantization-level noise; plus a single clean planet | quantization step, path count | zero promoted sources; reports the rate over seeds |
| W7 | `scale_24` | Behavior at game scale | 24 planets from the generator's radius range 20–40, several players, 100–300 shots, multi-seed | seed (≥ 8), shot budget | missing-planet curve over time; fit time and map age (watermark lag) per decision |
| W8 | `epoch_change` | Map change mid-capture | A `Planets` message mid-run replaces the map | change at shot k | belief reset; old evidence not reused; viewer shows both epochs |
| W9 | `endpoint_reason` | Why an endpoint is or is not a contact | Endpoints near player, battlefield margin, sample limit, too few samples, true contact | one case per reason | each endpoint gets the right classification once Q-END exists; otherwise recorded as expected labels for later |
| W10 | `prefix_replay` | Learning curve | Chronological prefixes of W7 | prefix length | missing/center-error/held-out RMSE vs shots |

Priority for the viewer's goal (finding where the biggest errors are): **W7** first (it is the only family at live scale), then **W1, W2, W3** (the three live error shapes), then **W4/W5** (association ambiguity), then W6 (guards against over-correcting), W8–W10 as the estimator session's pipeline allows.

## 3. Capture metadata the viewer needs (contract row Q-SCN)

**Current implementation status:** existing synthetic scenario captures now emit `name`, `family`, `defect_class`, fixture/training/trial `params`, `seed`, and `difficulty`; current `seed` and `difficulty` values are null because the fixtures have no recorded seed or objective ranking. Per-decision diagnostics emit `scenario.intended`. The metadata slice is complete for the existing fixtures. W1–W10 and D1–D7 experiment families, including seeded scale-24 runs, are still proposed and have not been implemented.

Additive fields in the scenario capture's `metadata_json`:

- `scenario`: `{ "name", "family" (W-id), "defect_class", "params": {…the swept values…}, "seed", "difficulty": integer rank within the family }`.
- Per decision, extend the existing `scenario.stage` with `scenario.intended`: truth planet indices that are *meant* to be unseen, displaced or mis-sized (so the viewer can mark "intended defect" in the disclosed-planet list and tell expected misses from surprises).
- Truth is recorded only for capture scoring/viewing (`Planets` plus the existing `truth_planets` diagnostic); the runner never submits it to `OnlineEstimator` or the targeting policy.

Absent fields mean "unlabeled"; the viewer must keep working on the 17 existing captures.

## 4. What the viewer will do with them (viewer session)

- Overview table: add `family`, `defect_class`, `seed` columns, group/filter by defect class, and report medians **per family** next to the global metrics.
- Disclosed-planet list: mark intended-defect planets; the missing-planet list then separates expected from surprising misses.
- Trend strip: already shows missing planets over decisions; a prefix-replay family (W10) becomes a learning curve there directly.
- No new viewer controls are needed for any family.

## 5. Ownership and sequence (proposal; estimator session to confirm or counter)

1. Estimator session decides whether the families map onto its hypothesis-experiment runner (Round 24) so the same specs feed both the viewer captures and its scorecards.
2. Scenario code needs one structural change before anyone can add families without touching the 1274-line binary: `Scenario`/`Launch` and the capture writer must be reachable from a second file (library module or `#[path]` include). That change is the estimator session's, because it has the file's uncommitted edits.
3. After that, the viewer session writes the family generators (pure functions returning `Scenario`) in a new file, claims `learning_scenarios.rs` only for the one-line registration, and releases the claim in the same commit.
4. Until then: no scenario code is touched by the viewer session.

## 6. Questions for the estimator session

- Do you already have, or plan, any of W1–W10 under other names? (Round 24 lists displaced known planet, wrong radius and one extra unseen planet.)
- Which of the pass criteria conflict with the metrics your runner reports?
- Is the generator's radius range (20–40) and the 24-planet count the right scale for W7, and which seeds should be frozen as the report split?
- Can `scenario.intended` be produced by the scenario definition itself (it knows which planets it hid)?

## 7. Added 2026-11-21 (user idea): degradation ladders from perfect information

The families in §2 start from a blank or partial belief and add evidence. The inverse is just as informative: start with the **truth map as the belief** and degrade it one factor at a time, measuring how targeting falls apart. This isolates which kind and size of error actually costs hits.

Evidence that the baseline is sound (viewer session, captures 41–44, disclosed maps): re-simulating each observed shot under the disclosed map reproduces the observed path to median 0.000 and max 1.3 units RMSE (311 of 312 shots under 1 unit), and 53 of 56 predicted victims did not move between decision and impact. So with perfect information, path prediction is exact and targets are almost always static: perfect-map targeting should be near-perfect, limited by search coverage (the policy on the disclosed map still returns `Approach` instead of `Hit` for 15 of 72 decisions) and by players that relocate (3 of 56).

Proposed measurement (oracle-evaluated hit): choose a shot with the **degraded** belief, then simulate that shot on the **true** map with the real player positions; it hits only if the true path reaches a player. No live run needed, fully deterministic.

| ID | `defect_class` | Degradation applied to the true map | Sweep |
| --- | --- | --- | --- |
| D1 | `drop_planet` | remove one planet (nearest to the shot, farthest, largest, smallest) | which planet; number removed 1..k |
| D2 | `displace_planet` | move one planet | d = 2, 5, 10, 20, 40 units, several directions |
| D3 | `resize_planet` | change radius (mass follows radius³) | ±2, ±5, ±10 units |
| D4 | `phantom_planet` | add a planet that does not exist | distance from path, radius |
| D5 | `stale_player` | targets move after the decision (positions from before) | displacement, delay |
| D6 | `noisy_map` | jitter every planet's position/radius with a given σ | σ ladder |
| D7 | `residual_only` | replace planets with the local residual correction fitted to the true field near observed paths | support radius, path count |

Per rung report: oracle-evaluated hit rate over a fixed set of target configurations, median miss distance, path RMSE of the chosen shot, and whether the policy still claims a hit (predicted-hit rate minus real-hit rate is the overconfidence gap, which is the number that matters for exploration).

Ownership follows §5: runner and metrics stay with the estimator session; the viewer session can add the same experiment to the shot review (select a decision, degrade the disclosed map, show the resulting path and miss) because it already has the repair machinery in the other direction.
