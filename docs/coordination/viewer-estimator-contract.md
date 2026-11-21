# Viewer ↔ estimator coordination

Two Claude sessions work in this checkout concurrently. The **viewer session** owns the map inspector (what a person sees and can explore). The **estimator session** owns how the bot learns, simulates and targets (what is true about the model). This file is their shared interface and protocol. Both sessions read it before touching anything the other depends on, and update it in the same commit as the change.

Last edited by: viewer session, 2026-11-21. Source of the requirements below: the user's Round 27 uncertainty decomposition in `docs/brainstorms/2026-11-13-newtonwars-bot.md` and the viewer decisions in `docs/brainstorms/2026-11-21-map-viewer-ux.md`.

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
| `bot_state` (positions, in-flight, covered cells) | decision | per decision | |
| `map_epoch`, `map_submitted_watermark`, `map_processed_watermark`, `map_pending_events`, `map_wait_ms`, `map_wait_fallback`, `map_updating` | decision | per decision | observation age |
| `choice` | decision | per decision | policy output |
| `scenario.stage` | decision (synthetic) | per decision | label only |

### Requested by the viewer (estimator to decide and answer)

| ID | Quantity | Granularity | Viewer use | Status |
| --- | --- | --- | --- | --- |
| Q-MD | `model_disagreement_at(Vec2) -> Option<f64>`: gravity spread between the no-source and validated source models, weighted by held-out improvement and independent validation count; `None` without a validated source | per location | inspect broad-area model-choice uncertainty separately from physical parameter covariance and residual uncertainty | library method implemented; viewer endpoint pending |
| Q-EV | evidence availability `evidence_at(Vec2) -> Option<f64>` reflecting global parameter sensitivity of observed paths, not a fixed-distance tube; `None` when no evidence model exists | per location | replaces the current binary "Evidence" layer (viewer currently uses only the 40-unit residual support) | proposed |
| Q-RU | residual uncertainty `residual_uncertainty_at(Vec2) -> Option<f64>` | per location | separate layer from residual mean | proposed |
| Q-FU | epistemic field uncertainty `field_uncertainty_at(Vec2) -> Option<f64>` derived from the model mixture (not a sum of the above), with a `components` breakdown | per location | combined view plus per-cause attribution | proposed |
| Q-COV | covariance availability: why `null` (`not_converged`, `rank_deficient`, `too_few_paths`) and, ideally, a defensible fallback | per belief | explicit unavailable reason on the Uncertainty layer | proposed (user Round 26) |
| Q-ALT | whole-map alternatives with weights/labels (no-source, mislocated, extra source) | per belief | compare alternatives against truth and against each other on the timeline | proposed |
| Q-HYP | hypothesis list with lifecycle: `proposed / provisional field-only / corroborated field-only / physical geometry supported / rejected`, support and held-out score | per hypothesis | timeline of hypothesis states; marker on map | proposed |
| Q-SIM | shot sensitivity `shot_sensitivity(belief, choice) -> {paths, outcome_disagreement, collision_margins}` | per shot candidate | draw path spread and attribute it to causes | proposed |
| Q-END | per-endpoint classification with reason (`accepted contact`, `near tracked player`, `battlefield margin`, `sample limit`, `too few acceleration samples`, ambiguous) | per `ShotFin` event | show why an endpoint was or was not a contact | proposed |
| Q-SCN | scenario metadata in capture metadata: `name`, `defect_class` (e.g. `missing_planet`, `displaced_planet`, `wrong_radius`, `extra_unseen`), seed, intended difficulty | per capture | sessions table; compare like with like | proposed |
| Q-PRED | pre-shot model forecast stored with each chosen shot (predicted path/outcome) so the observed path can be compared later | per shot | "forecast vs observed" view | proposed |

### Planned by the viewer (no estimator work needed)

| ID | Item | Status |
| --- | --- | --- |
| V-MATCH | one planet matching in `map_viewer.rs` (global nearest-first, ≤ 80 units) shared by `/api/snapshot` (`planet_match`), the decisions list (`accuracy`), metrics, planet lists; per-planet status `close / center off / size off / both / missing` (center > 10 units, radius > 5 units) with the numbers beside it | displayed |
| V-TREND | error-over-time lines in the timeline strip (estimated planets, missing disclosed planets, fit RMSE) | displayed |
| V-TABLE | captures table with global session metrics (missing planets, median center error, final fit RMSE, scenario/live labels); no single combined score | proposed |
| V-LAYERS | layer registry: each layer = one library method + a label + an availability message; adding Q-EV/Q-RU/Q-FU is one row each | proposed |

## 4. Active claims

| File | Session | Why | Since |
| --- | --- | --- | --- |
| *(none)* | | | |

## 5. Open requests

| # | From | To | Request | Status |
| --- | --- | --- | --- | --- |
| 1 | viewer | estimator | Answer the `proposed` rows above: which will be emitted, as which method, with which unavailable reasons. Prioritize Q-COV, Q-EV, Q-SCN (they unblock the earliest viewer value). | open |
| 2 | viewer | estimator | Scenario corpus ownership (user said "we build more scenarios"): proposed split — the viewer session writes **new scenario definitions** in a new file `src/bin/learning_scenarios/…` or an appended section only after claiming `learning_scenarios.rs`; estimator session keeps ownership of the scenario *runner* and metrics. Confirm or counter-propose. | open |
| 3 | estimator | viewer | Tell the viewer which belief states (physical-only vs residual-corrected, hypothesis variants) you still need to compare in the UI; the 2026-11-21 viewer rework removed the field/colour/scale/grid/arrows/coverage controls and keeps only: layers Gravity / Uncertainty / Evidence / Error, a "Local corrections" toggle, and an opt-in raw-residual overlay. Rows here become new layers via V-LAYERS, not extra settings. | open |

## 6. Changelog

| Date | Session | Change | Breaking? |
| --- | --- | --- | --- |
| 2026-11-21 | viewer | `/api/decisions` gains `accuracy` (disclosed-map comparison, null without a disclosed map) and `/api/snapshot` gains `planet_match`; both additive. | no |
| 2026-11-21 | viewer | Created this contract. Viewer removes the `field`/`heat`/`scale`/`resolution` controls; `/api/snapshot` keeps accepting `field` and `step` for compatibility. | no |
| 2026-11-21 | estimator | `field_hypothesis` adds support paths and validation/training metrics; joint hypotheses are seeded from prior trajectories and scored on new whole paths against the frozen prior physical map, then vetoed if the post-fit physical map already explains them. `MapBelief::model_disagreement_at` exposes a separate broad source/no-source gravity spread; it is heuristic, not calibrated, and does not replace residual uncertainty or full Q-FU. | no |
