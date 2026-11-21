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
