# Map viewer UI/UX (grill-me workstream)

Started 2026-11-21. Mode: sequential. Checkout policy: current local checkout (global git preference; no worktree needed).

## Goal / scope
User is unhappy with the amount of information provided/available/selectable in `web/map_viewer.html` + `src/bin/map_viewer.rs`. Make the viewer object-centric and navigable. Out of scope: estimator/targeting behavior.

- S2 (user, 2026-11-21): all non-UI/UX work (estimator, targeting, capture/recording, scenarios) is handled in a different concurrent session. Scope here: `web/map_viewer.*` and `src/bin/map_viewer.rs` only. Anything needing changes elsewhere (e.g. new fields in diagnostics) is an open flag for that session, not done here; preserve its uncommitted changes and avoid touching shared files.

## Facts (observed in code/data, 2026-11-21)
- Planets drawn as unlabeled circles; click shows nearest estimated/actual planet as a text dump. Players, contacts, hypothesis source not selectable.
- Decoded but never displayed: `planet_contact_counts`, `uncertain_associations`, `fit_converged`; covariance only used for the sigma heatmap.
- Controls: 8 fields x 5 color-by x 3 scales x 6 grid sizes + 9 checkboxes. Default `delta` needs an oracle map; live captures have none, UI silently falls back.
- Decision picker = `<select>` (168 entries in capture 42); timeline = one button per event (285 in capture 44); no prev/next/scrubber; compare/audit output = raw JSON.
- Diagnostics rows are 50-280 KB JSON each; a belief-history series must be summarized server-side, not shipped whole.
- JS is minified-style one-liners inside the HTML.

## User policies / settled decisions
- S1 (user, "go ahead with your suggestions where it's clear"): proceed with: object inspector (hover tooltip + pinned selection incl. planet list/cards), scrubber + prev/next/keys for time, fewer named layers with rest under "advanced", replace JSON dumps with tables, cut explanatory prose, split JS to `web/map_viewer.js` (no build step). Scope: this UI workstream. Does not accept any P task.

## Questions
- Q1 ready: primary use case / default layers (live no-oracle vs scenario with oracle).
- Q2 ready: scrubber unit (decisions vs all events; multi-bot).
- Q3 ready: belief-over-time (sparkline/planet tracking) — needs planet identity across snapshots.
- Q4 ready: which layers survive as first-class.

## Round 1
Session assessment (written before peer):
- Q1: design for live (no oracle) as default; oracle layers appear only when present. Scenario captures are the debugging ground truth but live is where unhappiness arises.
- Q2: one scrubber over the capture's full sequence with decision ticks highlighted; bot filter only dims/limits decision stepping.
- Q3: yes, server-side compact series (planet count, RMSE, per-planet contacts) with nearest-center matching between consecutive snapshots; show as sparkline under scrubber; skip per-planet history cards initially.
- Q4: Gravity (belief), Uncertainty (sigma), Evidence coverage, Error vs oracle (only if oracle); hypothesis layers + raw residual under advanced.

Peer: cross-model, codex -> gpt-6-luna (review cf5303306f4e4dbfb3a9f106dd16b986); attempted_falsifications non-empty (5).
Peer recommendations: Q1 live default, oracle comparison only when present, explicit unavailable state; Q2 decisions as primary axis + optional event stepper, shared estimator state shown as shared; Q3 aggregate trend (planet count, RMSE) only, NO cross-snapshot planet matching (false continuity); Q4 first-class = planet geometry, gravity, trajectories, players/contacts; rest advanced.
Divergences: Q2 (I proposed all-event scrubber w/ decision ticks; peer decisions-first). Q3 (I proposed nearest-center matching; peer rejects it). Revised: I accept peer on Q3 (matching invents identity; no evidence it is needed). Q2 kept as open option.
Impact: changed my Q3 decision; Q1/Q4 no decision impact; Q2 added an option.

User answers (2026-11-21): Q1 = A (live-first; oracle layers only when present, explicit unavailable state). Q2 = A (scrubber over all events, decision ticks highlighted, bot filter limits decision stepping; peer preferred decisions-first, user chose A). Q3 = A (aggregate trend strip only; per-planet estimation history deferred, owner: user, "maybe later"). Q4 = A (Gravity/Uncertainty/Evidence coverage/Error-if-oracle; trajectories, players, contacts as overlays; rest advanced; user: layer sprawl is the root of the clunkiness). P1 accepted.
Follow-ups now ready: P2 scrubber + trend strip (Q2, Q3); P3 layer reduction (Q4, Q1).

## Proposals
- P1 (ACCEPTED 2026-11-21, owner: main session writes code, reviewers after): split JS to web/map_viewer.js; object inspector (hover tooltip, click-to-pin for planet/player/contact/trajectory, planet list in sidebar showing radius, mass, contact count, sigma, oracle match+error); surface fit_converged/uncertain_associations; replace JSON dumps with tables. Files: web/map_viewer.html, web/map_viewer.js, src/bin/map_viewer.rs (serve JS, expose per-planet sigma). Non-goals: time navigation, layer reduction. Verify: cargo fmt --check, cargo test, manual run against capture 42 and a scenario capture. Stop: local commit.

### P1 progress (2026-11-21)
Implemented (uncommitted, pending review): JS split to web/map_viewer.js (served at /map_viewer.js); hit-test + tooltip + pinned selection for planet/actual planet/player/contact/shot/point (only drawn layers hit-testable); planet list table; fit_converged + ambiguous associations in metrics; compare/audit as key-value tables; per-planet sigma computed client-side from covariance (3 params/planet, null in live capture 44 so shown as unavailable). Verified in headless Chrome via CDP against capture 44 (no JS errors). cargo fmt --check + map_viewer tests pass (new test: page references served script). Reviews running: reviewer, simplicity-reviewer.
Open flag (other session): live captures' covariance is mostly null, so per-planet sigma shows 'unavailable' most of the time; that is an estimator-side matter.
Next ready: P2 scrubber + trend strip (Q2, Q3), P3 layer reduction (Q4, Q1).

P1 reviews: general reviewer found a real bug (pan release replaced the pinned selection) and planets being shadowed by trajectories in hit-testing: both fixed; sigma null guard, empty note row, tooltip clamp fixed. Simplicity reviewer: deduplicated nearest-planet/tooltip-point code. Skipped: locate() abstraction, inlining pointRows, 'unavailable' vs 0 for uncertain_associations on old captures (Rust decoder can't tell), stale grid for ~120ms after zoom. Re-verified in headless Chrome. README updated. P1 committed.

## P2 / P3 (ACCEPTED 2026-11-21 by user: "go ahead P2 & P3")
- P2 Time navigation (Q2 A, Q3 A). Replace decision `<select>`, 'No decision' button and per-event button wall with one scrubber canvas above the map: x = ordinal position in the capture's full event+decision timeline; decision ticks tall (bot-colored, dimmed when filtered out), shot-end/map-reset markers; trend strip above ticks (planet count, fit RMSE per decision; converged flag). Prev/next event and decision buttons + keys (arrows = event, shift+arrows = decision within bot filter, Home/End). Drag/click to scrub with debounced snapshot load; end of strip = latest state. Server: only add fit_converged and contact count to list_decisions (planets/RMSE already there). Non-goals: per-planet history (deferred), playback.
- P3 Layer reduction (Q4 A, Q1 A). Segmented primary layer: Gravity (default), Uncertainty, Evidence coverage, Error (only when disclosed map exists; unavailable state otherwise). Overlays visible: estimated planets, disclosed planets (oracle only), trajectories, contacts; players always. Everything else (full vector-field list, color-by, scale, grid, arrows, raw path residuals, bot coverage) under 'Advanced'. Decision/targeting panels shown only when a decision is selected. Cut explanatory prose to short notes. Non-goals: backend changes.
- Files for both: web/map_viewer.html, web/map_viewer.js, src/bin/map_viewer.rs (list_decisions fields only). Verify: fmt, tests, headless-Chrome CDP scripts on capture 44 and a scenario capture. Stop: local commits (one per P).

### P2/P3 progress (2026-11-21)
P2 committed (aedf314): scrubber + trend strip + stepping/keys; server list_decisions adds fit_converged and contacts. P3 implemented: Gravity/Uncertainty/Evidence/Error presets over the existing field/heat selects (Error disabled and disclosed overlays hidden without a disclosed map; explicit 'unavailable' message when a layer has no values), overlays reduced to planets/disclosed/trajectories/contacts, rest under Advanced, decision+targeting panel only with a decision selected, prose cut to a short Help list, oracle-only metrics hidden without oracle. README rewritten for time/layers/inspection. Verified in headless Chrome on capture 44 and on a temp copy with Planets events removed (no-oracle path); all existing captures include Planets events, so the no-oracle path was only exercised on that synthetic copy. Next: fresh-context reviews over b75e3d8..HEAD, then fix forward.
Deferred (owner: user, 'maybe later'): per-planet estimation history across snapshots.

P2/P3 reviews: cross-model diff review (codex -> gpt-6-luna) found the one real defect (late /api/timeline response from a previous capture could overwrite the current one); same-model reviewer confirmed it and added: guard loadDecisions too, clear timeline on capture change, handle pointercancel for scrub drag (all fixed), RMSE label precision (fixed), dropped unused `contacts` field. Rejected: keyboard handling with focused buttons (arrows don't activate buttons, navigation must work after clicking one), zero-width canvas (not reproducible), scrub resize (already redrawn on window resize), uncertainty preset being rewritten by fallback (false). Skipped nits: missing belief plotted as 0 planets, duplicate debounced load on End, decisionAt linear scan. Fixes committed after review.

## Round 2 (2026-11-21, after P1-P3 shipped)
User feedback: "better", but wants a *substantial reduction first*: many Advanced options are not useful as options, least of all unexplained. Also: uncertainty map should carry information much earlier; consider building more test scenarios; wants to learn/explore where the biggest errors still happen and the interface lacks the controls for that.
- Policy S3 (scope: viewer controls): default to REMOVING unexplained options rather than keeping them; keep only controls with a clear question they answer. Source: user reply above.
- Scope note vs S2: more scenarios and earlier covariance are estimator/scenario work (`learning_scenarios`, `online_map`), owned by the other session. Q9 asks how to hand that over.
- Other session (docs/brainstorms/2026-11-13..., Round 24) uses 'physical only vs residual-corrected' views and raw residual-sample overlay as debugging tools and plans hypothesis experiments; its follow-up list already names scenarios: displaced known planet, wrong radius, one extra unseen planet.

Facts observed this round (capture 44, live, disclosed map present; planets matched greedily within 80 units):
- Final belief: 18 estimated vs 24 disclosed; 6 missing (radii 20-32), median center error 3.4, worst matched center 41.4 (radius +1.9, mass x1.15), another match radius +10.1 / mass x2.39.
- Over decisions: missing planets 21 -> 18 -> 16 -> 15 -> 11 -> 9 -> 8 -> 6 as decisions progress; median center error 70 -> 4.
- fit covariance is null in every decision of capture 44 (and 42), so the Uncertainty layer has no data for the whole capture; fit_converged is false in 42, true at the end in 44; model_position_rmse is None for several mid-capture decisions and 267-315 late.
- Existing viewer error aids: Gravity error layer (field vector), planet match counts in metrics, Audit button (engine vs observed, worst shot only), Compare. No per-planet error status on the map, no missing-planet highlight, no error-over-time series, no per-shot belief-prediction ranking, no cross-capture overview.

Round 2 session assessment (before peer): see Q5-Q9 below; written before reading the peer result.
- Q5 remove: grid selector (auto), color scale (fixed robust), color-by X/Y, vector arrows, bot coverage, path residual dots; replace 'physical only vs corrected' by one explained checkbox 'Local corrections'; hypothesis shown as a marker only when one exists. Contested = raw residuals + hypothesis fields (other session uses them).
- Q6 error unit: planet status (matched/mislocated/size/missing), error over time, per-shot belief prediction error; field-error map stays as one layer.
- Q7 cross-capture overview table: yes, it directly answers 'where are the biggest errors'.
- Q8 merge Uncertainty+Evidence into 'Confidence': sigma where covariance exists, else distance-to-nearest-path proxy, labeled; ask other session why covariance is always null.
- Q9 scenario wishlist from this session, scenarios built by the other session.

Round 2 peer: cross-model codex -> gpt-6-luna; attempted_falsifications non-empty (5).
- Peer: Q5 replace Advanced with few named diagnostic controls, keep raw residual dots as an explicitly named opt-in overlay (other session uses them), fold physical-only vs corrected into a model comparison, drop bot coverage/scale/grid/arrows/color-by from the normal path. Q6 order: per-planet status + missing highlight, field error map, ranked per-shot belief error, error over time; targeting outcome later/separate. Q7 capture table as landing page, no single aggregate score. Q8 keep Uncertainty and Evidence separate; do not present a proxy as calibrated; label heuristics, show ingredients. Q9 viewer-derived wishlist to the scenario owner. Missing alternatives: ranked list beside the map, jump-to-metric-peak, data availability/definitions next to each diagnostic.
- Divergence: Q8 (I proposed a merged 'Confidence' layer with a distance proxy). Revised to peer's position: it conflicts with the project contract that missing covariance stays explicitly unavailable; keep Uncertainty strict, make Evidence richer instead. Q5: peer keeps raw residuals; I'd cut them; left as the user's call. Impact: changed my Q8 decision; added alternatives (ranked list, jump-to-peak); otherwise no decision impact.
Matching policy decision (mine, small, reversible): one shared global-nearest matching (smallest distance first, <=80 units) replaces estimate-order greedy in metrics/list/trend.

### Round 2 user answers (2026-11-21)
- Observation: layer presets changed hidden Advanced settings that did not fit. Resolution: the field/heat/scale/grid selects are removed; each layer is a self-contained mode.
- Q5 = B. Policy S4: debugging matters, but the current Advanced section has negative value; a later, reworked advanced section is not ruled out. Keep: Gravity/Uncertainty/Evidence/Error layers, a 'Local corrections' toggle, an explained opt-in raw path-residual overlay, a field-only-source marker when one exists. Remove: field list, color-by, scale, grid, arrows, bot coverage.
- Q6: wants the whole epistemic chain explorable: evidence availability, field uncertainty / planet hypothesis, residuals, simulation stability and their downstream effects, including combined belief states and comparison to actual, over the timeline. The estimator session is reworking those parts; its notes (Round 27) define the decomposition and say the viewer should offer individual layers and useful combined views. Handled by docs/coordination/viewer-estimator-contract.md (user will tell the other session where it is).
- Q7 = capture/session table, plus global session metrics where computable. (P6 candidate.)
- Q8 = agree; explained by Q6, the estimator notes and implementation (viewer keeps Uncertainty strict; richer Evidence arrives through contract row Q-EV).
- Q9 = 'we build more scenarios'. Interpreted as: scenarios get built (viewer-informed); ownership split proposed in the contract (open request 2) because S2 says the other session owns estimator/scenario code. (P7 candidate; not started.)
- Accepted: P4 (reduction), P5 (planet status + ranked list), coordination doc (written: docs/coordination/viewer-estimator-contract.md).

### P4/P5 (commit 4f42592) and review fixes
Implemented: Advanced removed; layers are self-contained client-side modes (server always asked for field=total; client derives gravity/physical/error from per-sample total/physical/oracle); 'Local corrections' toggle; opt-in 'Path residuals' overlay; field-only source marker; one server-side matching (`match_planets`, closest-first, one-to-one, <=80 units) exposed as `planet_match` (snapshot) and `accuracy` (decisions); ranked disclosed-planet list with status (center>10 / radius>5 units), status-colored disclosed planets on the map, missing-planet trend line, map-age rows. Coordination contract committed: docs/coordination/viewer-estimator-contract.md (user informs the other session).
Reviews (cross-model gpt-6-luna + same-model reviewer): fixed 'no belief reported as all planets missing' (accuracy/planet_match null, status 'unknown'), disclosed map newer than the belief's snapshot (planet_match null), bad Planets row no longer breaks /api/decisions, selection reset on refresh, proper even-count median, Gravity error metric follows the corrections toggle, stale comment/README wording. Kept: greedy closest-first matching (intentional; test documents that it is not maximum-cardinality). Skipped: thresholds in Rust (JS only today), step-line construct, hypothesis marker ignoring the planets toggle.
Open for next round: P6 capture/session table with global metrics (Q7); P7 scenarios (Q9; blocked on contract open request 2); layer/stability views wait on contract rows Q-EV/Q-RU/Q-FU/Q-SIM.

### P6 (started 2026-11-21; user: 'proceed' after P6 offer)
Captures overview as the landing view: `/api/overview` (per capture: kind live/scenario from metadata host, name, revision, event/shot/decision counts, last decision's planets/fit RMSE/converged/disclosed-map accuracy via the shared matching) + a sortable table with a kind filter and global session metrics (totals; per-kind medians of final missing share, median center error, fit RMSE). No combined score. Row click opens the capture at its latest state. Non-goals: per-decision aggregates (would parse every diagnostic), jump-to-worst-moment (later). Contract note: estimator session holds a claim on docs/coordination/viewer-estimator-contract.md (additive residual/model-disagreement diagnostics); viewer must not edit it until released; update V-TABLE row after.
P7 remains blocked on contract open request 2.

P6 committed (562ee10) + fixes: cross-model (codex gpt-6-luna; the OpenCode peer failed to run, reported not fatal) and same-model review both found the real bug: opening a row for a capture missing from the capture select (new capture or startup race) loaded id 0. Fixed: openCapture refreshes the select; startup no longer loads the newest capture behind the hidden view; accuracy computed independently of the other 'last' fields; DB errors propagate; keyboard shortcuts disabled and Refresh reloads the table in overview mode; unused events count dropped. Skipped: N+1 queries (44 captures, 0.3 s), loadOverview staleness guard. Open flag: contract row V-TABLE should become 'displayed' once the estimator session releases its claim on the contract file.
