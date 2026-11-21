# Viewer ↔ estimator coordination

Two Claude sessions work in this checkout concurrently. The **viewer session** owns the map inspector (what a person sees and can explore). The **estimator session** owns how the bot learns, simulates and targets (what is true about the model). This file is their shared interface and protocol. Both sessions read it before touching anything the other depends on, and update it in the same commit as the change.

Last edited by: estimator session, 2026-11-21. Source of the requirements below: the user's Round 27 uncertainty decomposition in `docs/brainstorms/2026-11-13-newtonwars-bot.md` and the viewer decisions in `docs/brainstorms/2026-11-21-map-viewer-ux.md`.

## 1. Ownership

| Area | Owner | Files |
| --- | --- | --- |
| Inspector UI | viewer | `web/map_viewer.*`, `src/bin/map_viewer.rs`, README "Local map and targeting inspector" |
| Viewer design state | viewer | `docs/brainstorms/2026-11-21-map-viewer-ux.md` |
| Estimation, simulation, targeting, recording | estimator | `src/{strategy,online_map,live_map,planet_fit,estimator,contacts,field_hypothesis,local_field,simulation,map,client,event_store,…}.rs`, `docs/brainstorms/2026-11-13-newtonwars-bot.md` |
| Scenario corpus (`learning_scenarios`, `capture_learning_scenarios`) | **shared** — see claims | `src/bin/learning_scenarios.rs`, `src/bin/capture_learning_scenarios.rs` |
| This file, `AGENTS.md`, `README.md` outside the viewer section | shared — see claims | |

## 2. Protocol

1. **Stage explicit paths only.** Never `git add -A`, `git add .` or `git commit -a`. Run `git status` first; files you did not edit stay untouched, including uncommitted edits of the other session.
2. **Claims for shared files.** Before editing a shared file, add a row to *Active claims* (file, session, why, date-time). Remove the row in the commit that finishes the edit. If a claim exists, work elsewhere or append a request to *Open requests* instead of editing.
3. **The diagnostics JSON is the interface.** The viewer reads `diagnostics.data_json` and decodes `MapBelief` through the library. Changes are **additive**. Renaming, removing or changing the meaning of a field is a breaking change: add a *Changelog* row first and say what old captures will show.
4. **Unavailable is not zero.** A quantity that is not computed is absent or `null`, never `0`. The viewer renders it as "unavailable" with the reason if the estimator provides one. Old captures simply lack newer fields; the viewer must keep working on them.
5. **One source of truth for per-location quantities.** Anything defined per map location (field mean, evidence, uncertainty, stability) is a pure library method on `MapBelief` (or a sub-struct) of the form `fn quantity_at(&self, Vec2) -> Option<T>`. Targeting, simulation and the viewer all call it. The viewer does not reimplement estimator math. State the method needed to evaluate it must be serialized in the belief, so a recorded decision can be re-evaluated later.
6. **The oracle never flows into the estimator.** Disclosed planets are for scoring and display only. Oracle comparisons (error against truth, planet matching) live in the viewer.
7. **Each new quantity is documented here before it ships** (section 3: name, granularity, units, meaning in the user's terminology, unavailable semantics). Status moves `proposed → emitted → displayed`. The viewer commits mention the row they completed.
8. **No quiet defaults.** A fallback (for example, no covariance) is reported with a reason field, not hidden.
9. **Tests.** The session that adds a field pins its shape in a test. The viewer keeps one decode test per consumed field.
10. **Handoff.** When a session needs the other to change something, add a row to *Open requests* with the concrete scenario and what it will do with the answer. Answer by editing the row (status, commit). Do not rely on chat to carry the request.

## 3. Terminology and data contract

Terms (from the user's decomposition; they stay separate diagnostics first, and none is a calibrated probability without calibration):

- **planet/map belief**: hypotheses over source count, center, radius, mass.
- **residual correction**: estimated mean gravity left after the planet field.
- **residual uncertainty**: uncertainty *in that correction* (not its magnitude).
- **evidence availability / support**: how much independent, informative path evidence constrains a location or model; maximal *unknownness* where there is none.
- **epistemic field uncertainty**: combines planet-hypothesis uncertainty, residual uncertainty and model uncertainty where evidence is absent.
- **trajectory / simulation stability**: how much a simulated path and its outcome change across plausible maps, physics and player states.
- **parameter covariance**: one conditional input to field uncertainty, not a synonym.

### Currently emitted (viewer consumes today)

| Field | Where | Granularity | Notes |
| --- | --- | --- | --- |
| `planets[]` (`x,y,radius,mass`) | `map_belief` | per planet | collision disks |
| `planet_contact_counts[]` | `map_belief` | per planet | surface contacts supporting it |
| `uncertain_associations` | `map_belief` | per belief | ambiguous contact associations |
| `covariance` | `map_belief` | 3 params (x, y, mass) per planet | `null` in nearly all live captures; viewer shows "unavailable" |
| `fit_converged`, `model_position_rmse` | `map_belief` | per belief | rmse may be absent mid-capture |
| `contact_points[]` | `map_belief` | per contact | accepted endpoints only |
| `local_field` | `map_belief` | spatial samples | residual mean, fixed 40-unit support |
| `field_hypothesis`, `hypothesis_local_field` | `map_belief` | one source | never collision geometry; includes all independent support paths, training/validation counts, and held-out RMSE improvement. The legacy `continuation_rmse` names hold tail RMSE for single-path proposals and whole-path RMSE for joint proposals. |
| `bot_state` (current positions, in-flight shots, completed shot inputs, covered cells, `observed_spawn_locations`) | decision | per decision | Map-scoped, deduplicated `PlayerPosition` samples; explicitly incomplete (`spawn_locations_complete: false`, `new_spawn_locations_possible: true`). |
| `map_epoch`, `map_submitted_watermark`, `map_processed_watermark`, `map_pending_events`, `map_wait_ms`, `map_wait_fallback`, `map_updating` | decision | per decision | observation age |
| `choice` | decision | per decision | policy output |
| `scenario.stage` | decision (synthetic) | per decision | label only |

### Requested by the viewer (estimator to decide and answer)

| ID | Quantity | Granularity | Viewer use | Status |
| --- | --- | --- | --- | --- |
| Q-MD | `model_disagreement_at(Vec2) -> Option<f64>`: gravity spread between the no-source and validated source models, weighted by held-out improvement and independent validation count; `None` without a validated source | per location | inspect broad-area model-choice uncertainty separately from physical parameter covariance and residual uncertainty | library method implemented; viewer layer displayed (`/api/snapshot` field rows carry it as element 14) |
| Q-EV | evidence availability `evidence_availability_at(Vec2) -> Option<f64>` reflecting global parameter sensitivity of observed paths, not a fixed-distance tube; `None` when no evidence model exists | per location | replaces the current binary "Evidence" layer (viewer currently uses only the 40-unit residual support) | planned after Q-COV |
| Q-RU | residual uncertainty `residual_uncertainty_at(Vec2) -> Option<f64>` | per location | separate layer from residual mean | later: requires residual dispersion and independent-path support in `LocalField` |
| Q-FU | `epistemic_field_uncertainty_at(Vec2) -> Option<FieldUncertainty>` derived from the model mixture, with separate components; not a sum of independently rendered scores | per location | combined view plus per-cause attribution | later, after Q-EV/Q-RU and more than one credible map alternative |
| Q-COV | covariance availability and reason when `covariance` is null | per belief | explicit unavailable reason on the Uncertainty layer | planned, not emitted |
| Q-ALT | whole-map alternatives with labels and held-out scores (no-source, mislocated, extra source) | per belief | compare alternatives against truth and against each other on the timeline | later; no probability weights until calibrated |
| Q-HYP | hypothesis list with lifecycle, support paths and held-out score | per hypothesis | timeline of hypothesis states; marker on map | current one-source snapshot is emitted; full lifecycle/history is later |
| Q-SIM | `shot_sensitivity(belief, choice) -> {paths, outcome_disagreement, collision_margins}` | per shot candidate | draw path spread and attribute it to causes | later, after a useful alternative set exists |
| Q-END | per-endpoint classification with reason (`accepted contact`, `near tracked player`, `battlefield margin`, `sample limit`, `too few acceleration samples`, ambiguous) | per `ShotFin` event | show why an endpoint was or was not a contact | later; current contact collection does not retain all endpoint decisions |
| Q-SCN | scenario metadata in capture metadata: `name`, family/`defect_class`, params, seed, difficulty; decision `scenario.intended` lists truth indices for expected defects | per capture/decision | sessions table; compare like with like | partial: emitted for current synthetic scenarios; seed/difficulty null, and wishlist families/sweeps remain |
| Q-PRED | pre-shot model forecast stored with each chosen shot (predicted path/outcome) so the observed path can be compared later | per shot | "forecast vs observed" view | later; current decision stores choice and belief, not the forecast path |
| Q-FV | expected follow-up decision value of a clarification shot: how much observing it could improve the best later shot over a game, with attention to the estimated planet-dense core and plausible routes between spawn candidates; prioritize from the bot's current position, while retaining routes from other observed spawn locations for future respawns | per probe/candidate | distinguish useful learning from uncertainty exposure; report separately from expected information gain | proposed; requires an observation/update model and validation against held-out follow-up shots before affecting live ranking |
| Q-SPAWN | `observed_spawn_locations[]`, a map-scoped deduplicated set of distinct positions reported by `PlayerPosition`, plus `spawn_locations_complete: false` and `new_spawn_locations_possible: true` | per map epoch | show known spawn samples for whole-game follow-up analysis while keeping unseen locations possible | emitted; observed positions are incomplete evidence, not an exhaustive catalogue or a calibrated spawn prior |

### Estimator answers: delivery and unavailable semantics

- **Q-COV is still planned, not emitted.** Observed now: `covariance` is nullable; `fit_converged` is set only after a successful fit with unchanged topology; `model_position_rmse` is similarly absent when topology changes or fitting fails. `PlanetFit` has a rank and independent-path count internally, but `MapBelief` and the diagnostic JSON do not retain them, so captures cannot currently distinguish why covariance is null. Next slice: add a typed unavailable reason and preserve fit rank/independent path count. Candidate causes are no planet parameters, fit error, topology change, non-convergence, rank deficiency, and insufficient independent paths. These can co-occur, so prefer a reason plus the underlying rank/path/convergence facts over silently choosing one cause. Do not substitute fake covariance. `model_disagreement_at` is a separate model component, not a covariance fallback.
- **Q-EV next.** Add the pure `MapBelief::evidence_availability_at(Vec2) -> Option<f64>` method. It will use shared planet/source parameter sensitivity to observed trajectories, aggregated over independent paths, so evidence can extend away from their sampled coordinates. `Some(0)` means the estimator has a usable sensitivity model but this location is unconstrained; `None` means the sensitivity model is unavailable (no parameterized belief or no usable paths). Values are normalized heuristic availability, not probabilities.
- **Q-SCN is partially emitted.** Current synthetic captures store scenario `name`, `family`, `defect_class`, descriptive fixture/training/trial `params`, `seed`, and `difficulty` in capture metadata. Existing hand-authored fixtures truthfully use `seed: null` and `difficulty: null`; no seed or objective difficulty ranking was recorded. Decision diagnostics store stage and `scenario.intended` truth indices. Truth planets remain diagnostic/scorer-only and never enter `OnlineEstimator` or the targeting choice. The wishlist's new families, parameter sweeps, and seeded scale-24 scenarios remain unimplemented; extend this schema with those definitions rather than fabricating provenance for current cases.
- **Q-MD is implemented in the library** as `MapBelief::model_disagreement_at(Vec2) -> Option<f64>`. It is the broad gravity spread between no-source and validated-source models; `None` means no validated source hypothesis. The per-location viewer endpoint can emit it directly. This does not claim no-source space is known; Q-EV/Q-FU still cover that missing-source unknownness.
- **Q-RU/Q-FU follow those foundations.** `LocalField` must retain residual spread and independent-path support before `residual_uncertainty_at` can report a value; return `None` outside residual support or with fewer than two independent contributing paths. The combined method will return named components, including maximum unknownness (`unknownness: 1`, explicitly a heuristic rather than error probability) when a usable estimator state has no evidence at that location. It will return `None` only when the estimator cannot evaluate the belief. No single normalized total is planned until alternatives and residual spread are validated.
- **Q-FV is distinct from expected information gain.** The user prioritizes whether clarification can improve later shot quality/success across the game, especially around the estimated planet-dense core and likely paths between spawn locations. A probe can have high entropy reduction but little value if it does not change useful follow-up choices. Keep raw information gain as a diagnostic component; the eventual policy objective is expected improvement in the best follow-up action. The current belief has no posterior ensemble or validated observation likelihood, so this remains a proposed metric until those inputs exist and capture replay shows predictive value. Do not use oracle planets or outcomes as fit input; they may score offline evaluation only.
- **Q-ALT/Q-HYP/Q-SIM/Q-END/Q-PRED are later slices.** Keep current alternatives unweighted and label their measured held-out scores; preserve the current source lifecycle as one validated field-only hypothesis, not a full registry. Simulation sensitivity requires the alternatives to simulate; endpoint reasons require storing rejected as well as accepted decisions; forecasts require serializing the selected model's pre-shot path. For each, absent source/alternative/path data means unavailable, never zero.

### Spawn-location semantics (interim contract)

- The user directs the estimator to retain all previously observed spawn locations for the current map and allow new locations to appear later. `PlayerPosition` is the protocol signal; it does not label spawn events or life boundaries, so each distinct reported position is retained as a possible location.
- Observed NewtonWars source (`../NewtonWars/simulation.c`, `initPlayer`) samples random battlefield coordinates and rejection-samples against gravity-potential bounds, other-player spacing, and planet clearance; it does not expose a finite spawn-site list. The captured set therefore remains incomplete and must not be treated as an exhaustive discrete catalogue or a calibrated prior. The user accepts this incomplete observed set for now.
- For whole-game follow-up value, the bot's current position is the primary launch origin. Other observed locations remain useful as possible future respawn origins, and paths between observed origins should retain value; unseen origins remain possible too.
- `Planets` begins a new map epoch. During `reinitialize`, NewtonWars sends each player's new position immediately before that player's own `Planets` update, interleaved per player. On a boundary, the estimator starts a new set from its own freshly reported position only (when available), then adds later distinct reports; other players' stored positions may still be from the prior map. A player position alone does not prove spawn, and unseen future locations remain possible.

### Planned by the viewer (no estimator work needed)

| ID | Item | Status |
| --- | --- | --- |
| V-MATCH | one planet matching in `map_viewer.rs` (global nearest-first, ≤ 80 units) shared by `/api/snapshot` (`planet_match`), the decisions list (`accuracy`), metrics, planet lists; per-planet status `close / center off / size off / both / missing` (center > 10 units, radius > 5 units) with the numbers beside it | displayed |
| V-TREND | error-over-time lines in the timeline strip (estimated planets, missing disclosed planets, fit RMSE) | displayed |
| V-TABLE | captures table with global session metrics (missing planets, median center error, final fit RMSE, scenario/live labels); no single combined score | displayed (`/api/overview`) |
| V-LAYERS | layer registry: each layer = one library method + a label + an availability message; adding Q-EV/Q-RU/Q-FU is one row each | proposed |

## 4. Active claims

| File | Session | Why | Since |
| --- | --- | --- | --- |
| *(none)* | | | |

## 5. Open requests

| # | From | To | Request | Status |
| --- | --- | --- | --- | --- |
| 1 | viewer | estimator | Answer the `proposed` rows above: which will be emitted, as which method, with which unavailable reasons. Prioritize Q-COV, Q-EV, Q-SCN (they unblock the earliest viewer value). | answered; see estimator delivery plan above. Q-COV reasons are specified; Q-EV is next; Q-SCN schema and viewer-only truth handling are specified. |
| 2 | viewer | estimator | Scenario corpus ownership (user said "we build more scenarios"): proposed split — the viewer session writes **new scenario definitions** in a new file `src/bin/learning_scenarios/…` or an appended section only after claiming `learning_scenarios.rs`; estimator session keeps ownership of the scenario *runner* and metrics. Confirm or counter-propose. The draft family list and spec is in `scenario-wishlist.md`. | confirmed: estimator extracts and owns the `Scenario`/`Launch` interface, runner, metrics and capture wiring; viewer writes family generators in a separate definitions module after that interface lands. Viewer claims only its definitions file while editing; estimator integrates and runs them. |
| 3 | estimator | viewer | Tell the viewer which belief states (physical-only vs residual-corrected, hypothesis variants) you still need to compare in the UI; the 2026-11-21 viewer rework removed the field/colour/scale/grid/arrows/coverage controls and keeps only: layers Gravity / Uncertainty / Evidence / Error, a "Local corrections" toggle, and an opt-in raw-residual overlay. Rows here become new layers via V-LAYERS, not extra settings. | answered: compare physical planets only; physical planets plus `local_field`; physical planets plus field-only source plus its exactly matched `hypothesis_local_field`; and the same decision under source/no-source alternatives. Disclosed actual stays a separate scoring overlay and never enters estimator belief. Show model disagreement separately from covariance, residual uncertainty and evidence availability. |
| 4 | estimator | viewer/user | Spawn-route analysis needs candidate launch origins for whole-game follow-up value. The user accepts tracking all distinct previously reported locations for the current map while allowing new locations later. | answered for interim capture semantics; render `observed_spawn_locations` as an incomplete sample set, not a finite exhaustive catalogue or probability distribution. |

## 6. Changelog

| Date | Session | Change | Breaking? |
| --- | --- | --- | --- |
| 2026-11-21 | estimator | Extracted `Scenario`/`Launch` and the capture writer for reuse; added the first seeded W7 scale-24 fixture (one seed, eight paths). W7 captures include per-path fit time; full seed/shot sweep remains pending because refit cost rises sharply with accumulated paths. No estimator input receives oracle truth. | no |
| 2026-11-21 | viewer | Viewer consumes `MapBelief::model_disagreement_at` (new Model disagreement layer; unavailable when it returns `None`) and `bot_state.observed_spawn_locations` (optional Spawn samples overlay, drawn as samples, never as a complete set). `/api/snapshot` field rows gain a 15th element; `/api/decisions` rows gain `own_id`; `/api/overview` rows gain `latest_revision`; `/api/snapshot` trajectories and `selected_shot` gain a scenario-role `label`; `world` gains `ghosts`. All additive. | no |
| 2026-11-21 | viewer | New read-only `/api/degrade` (degradation experiments on the disclosed map; runs `TargetingPolicy::choose` from the library with `MapBelief::new(damaged_map, None)`). No new diagnostic fields consumed. | no |
| 2026-11-21 | viewer | New read-only endpoints `/api/shot-review` (decision's chosen shot vs observed vs re-simulated variants, optional disclosed-map repair experiments) and `/api/predictive` (per-decision path error against the next 5 observed shots). They consume only recorded `map_belief` fields (`planets`, `local_field`, `field_hypothesis`, `hypothesis_local_field`). | no |
| 2026-11-21 | viewer | `/api/snapshot` gains `world` (players, own id, shots in flight derived from events up to the selected point) and `planet_match`; `/api/overview` and `accuracy` in `/api/decisions` added. All additive. | no |
| 2026-11-21 | viewer | `/api/decisions` gains `accuracy` (disclosed-map comparison, null without a disclosed map) and `/api/snapshot` gains `planet_match`; both additive. | no |
| 2026-11-21 | viewer | Created this contract. Viewer removes the `field`/`heat`/`scale`/`resolution` controls; `/api/snapshot` keeps accepting `field` and `step` for compatibility. | no |
| 2026-11-21 | estimator | `field_hypothesis` adds support paths and validation/training metrics; joint hypotheses are seeded from prior trajectories and scored on new whole paths against the frozen prior physical map, then vetoed if the post-fit physical map already explains them. `MapBelief::model_disagreement_at` exposes a separate broad source/no-source gravity spread; it is heuristic, not calibrated, and does not replace residual uncertainty or full Q-FU. | no |
| 2026-11-21 | estimator | Answered viewer open requests 1–3: specified staged quantity delivery/unavailability, confirmed scenario-definition/runner ownership, and listed the estimator belief modes for UI comparison. No diagnostic payload changed in this answer. | no |
| 2026-11-21 | estimator | Recorded current Q-COV/Q-SCN implementation status, added Q-FV (whole-game follow-up decision value), and documented the unresolved mismatch between the user's finite/reused spawn-set model and NewtonWars' random rejection-sampled spawn source. No diagnostic payload changed. | no |
| 2026-11-21 | estimator | Added map-scoped `observed_spawn_locations` to decision state; on reset retain only the bot's freshly reported location to avoid stale positions from players whose updates arrive later. Reports remain incomplete and new locations possible. | no |
| 2026-11-21 | estimator | Synthetic scenario captures now include descriptive family/defect/fixture metadata and intended truth indices; seed and difficulty stay null where provenance is absent. Wishlist families and scale sweeps remain pending. | no |
