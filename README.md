# Einsteinbot

Rust client and eventual information-gathering bot for [NewtonWars](https://github.com/Draradech/NewtonWars).

## Development builds

The Cargo `dev` profile uses `opt-level = 1` for Einsteinbot and `opt-level = 2` for dependencies. This is a modest optimization intended to make local CPU-heavy fitting less sluggish without applying maximum optimization across the dependency graph; we have not benchmarked it against the previous profile yet. Debug information and incremental compilation keep Cargo's defaults. Cargo notes that higher optimization can improve runtime at the cost of compile time, and recommends measuring levels for the project rather than assuming level 3 wins ([profile reference](https://doc.rust-lang.org/cargo/reference/profiles.html)).

Use the normal dev profile (`cargo run ...`) for local bot sessions, map inspection, and scenario tools. Use `cargo check` for quick type and borrow checking and `cargo test` for behavior checks. `cargo check` skips final code generation, so it is quicker but does not report every code-generation error ([command reference](https://doc.rust-lang.org/cargo/commands/cargo-check.html)). Use `--release` only when comparing optimized runtime or measuring performance.

We have not added a faster linker or reduced debug information. This checkout runs on macOS, and neither build timing nor debugger needs have been measured well enough to justify those tradeoffs. If build latency becomes a problem, first separate clean-build, incremental-build, and link time. Reduced debug info (for example `debug = "line-tables-only"`) is a candidate if debug-info generation dominates, but it removes variable-level debugger detail. A platform-specific linker is worth trying only if linking is a measured bottleneck; don't add Linux-only `mold` settings to this macOS project. Keep `opt-level = 3` for a benchmarked case, not as a blanket dev default.

## Diagnostic client

Each bot session opens one TCP connection. By default, one invocation starts one session; `--clients N` starts a local manager with N sessions. It reads the server's text welcome prompt, enters bot protocol 8, prints parsed game events, and requests energy once per second. By default it is passive; `--dummy` enables small randomized shots when reported energy is at least 2.

```sh
cargo run -- --name Einstein
```

```sh
cargo run -- --dummy
```

By default the client connects to `127.0.0.1:3490`, chooses a random name from a curated list of well-known scientists (excluding Newton), and records parsed events to `einsteinbot.sqlite3`. To disable recording:

```sh
cargo run -- --no-record
```

The database stores one capture per invocation. Each parsed shared world event is recorded once, and each bot's own events and semantic commands are tagged by bot name. Events get a global sequence number and timestamp. Raw TCP bytes are not stored. For a different database file, use `--db-path PATH`.

Capture metadata includes the Git revision embedded when the binary was built and `source_changes`, which records whether `src`, `Cargo.toml`, `Cargo.lock`, or `build.rs` had tracked or untracked changes at build time. Unrelated files are excluded; either value can be null when Git was unavailable. This identifies the build, not the checkout at execution time.

The `diagnostics` table records each targeting decision with the same per-capture sequence counter as protocol events. Its JSON includes energy, the bot's player and shot history, spatial coverage, the complete map belief and covariance used, whether a fit was pending, search duration, and the selected choice (or null when the policy falls back). To reconstruct ordering, join or union `diagnostics` and `events` by capture ID and sequence. Existing captures have no such rows.

Custom names must be printable ASCII and at most 15 bytes, matching the NewtonWars server limit.

Options can come from a TOML file, environment variables, and command-line arguments. Later sources override earlier ones in this order: config file, environment, command line. `--clients N` starts a local manager with N concurrent loopback connections (maximum 16); multiple clients are rejected for non-loopback hosts because NewtonWars blocks same-IP remote duplicates. The clients use the same targeting policy with 8°, 10°, and 12° angle-step variants. For example:

```toml
# einsteinbot.toml
host = "127.0.0.1"
port = 3490
name = "Einstein"
record = true
db_path = "einsteinbot.sqlite3"
dummy = false
targeting = false
oracle_map_for_policy = false
clients = 1
```

```sh
cargo run -- --config einsteinbot.toml
```

Set individual values with `EINSTEINBOT_HOST`, `EINSTEINBOT_PORT`, `EINSTEINBOT_NAME`, `EINSTEINBOT_RECORD`, `EINSTEINBOT_DUMMY`, `EINSTEINBOT_TARGETING`, `EINSTEINBOT_ORACLE_MAP`, `EINSTEINBOT_CLIENTS`, and `EINSTEINBOT_DB_PATH`. Set `EINSTEINBOT_CONFIG` to choose a config file through the environment. Recording defaults to on; set `record = false` or `EINSTEINBOT_RECORD=false`, or pass `--no-record` to disable it. Dummy actions default to off; set `dummy = true`, `EINSTEINBOT_DUMMY=true`, or pass `--dummy` to enable them.

The dummy chooses an integer velocity between 2 and `min(8, floor(energy))` and an integer angle from -180 through 180, at most one shot per energy response. Every outbound command, including connection setup and energy requests, passes through a client-side token bucket capped at 16 wire bytes per second with at most 512 bytes of burst. The upstream server starts each connection with a 512-byte allowance and replenishes one byte every three game-loop iterations. That is nominally 20 bytes per second at 60 loops per second; its actual limit changes with loop pace. NewtonWars exempts local connections from enforcement, but this client applies its stricter cap locally too.

Targeting is opt-in with `--target` or `targeting = true`. It scans legal integer velocities up to the available energy and the server speed limit, using 8°, 10°, or 12° angle steps and refining promising angles to 1°. The simulator checks planet and player collisions at physics substeps using the latest known player positions. It ranks predicted hits by centrality first and uses lower velocity only to break ties; current maps and hit predictions are too unreliable to optimize energy first. Player positions are snapshots and can move before impact; a nominal hit is not a calibrated hit probability. If it sees no opponent or finds no usable action, it falls back to the random dummy. An information bonus is considered only when no predicted hit is available and the average field uncertainty along the path clears a threshold. This bonus uses full fitted parameter covariance to propagate local physical-field variance, but remains an uncertainty-exposure proxy, not a full posterior information-gain calculation. Because missing covariance currently makes this signal unavailable, the policy has no uncertainty-derived information score in those cases; absence of covariance does not mean there is little to learn. Completed trajectories from all players populate a 20-unit spatial coverage grid, reset on each `Planets` update. The policy discounts probes by their fraction of new cells and adds up to 40 world units of miss-distance penalty to covered non-hit approaches. Predicted hits still take priority. This is a diversity heuristic, not evidence of higher hit probability. `TargetChoice` logs include covariance availability, covered fraction, and whether a known contact is near the predicted route. The settled policy rule is that possible contacts do not veto shots, since impacts can provide useful observations; however, current ranking still deprioritizes paths passing near a recorded contact point, so the implementation does not yet fully match that rule. Missing covariance is reported as unavailable and cannot trigger the uncertainty branch.

Completed paths also supply local gravity observations. The estimator subtracts the current physical map and interpolates the residual only near observed paths; outside that support the residual is unavailable. It may retain a separate field-only source hypothesis when it predicts withheld observations, including in a partial map when independent paths corroborate it. Such a source is not a planet or collision surface. Targeting searches with and without it and checks a shortlist of predicted hits against the alternative model. The recorded support fraction, alternative-model agreement and distance-based uncertainty proxy are heuristics, not calibrated probabilities. A hypothesis can be wrong even close to observed paths.

The canonical connection feeds player positions and completed trajectories to a background physical-planet estimator. Probable planet contacts seed position and radius estimates, with mass tied to the game's radius-cubed rule. The estimator associates compatible contacts and refines known planets jointly from regular trajectory samples, surface contacts and free-space constraints; the resulting partial map contributes gravity to targeting. Overlapping ambiguous hypotheses are retained as uncertain rather than counted twice. It uses no disclosed planet values as fit input.

Contact classification and planet fitting assume NewtonWars source defaults, including player radius 4, the 2,000-sample trail limit and generated planet radii 20–40. The server does not disclose all of these settings. Covariance is a local approximation and is unavailable when the fit is rank-deficient, has too few independent paths, or does not converge; it does not describe missing planets or uncertain associations. `Planets` messages define map epoch boundaries and clear prior evidence. Without them, changes of map cannot currently be detected reliably. The bot continues firing with the latest published belief while fitting proceeds, using a flat field before the first fit; a map reset clears the old belief. Finite-difference calculations within each fit run in parallel, while observations and fits stay ordered. `map_updating=true` in `TargetChoice` means the shot used a belief that may lag behind the most recent observations. `--oracle-map` explicitly uses disclosed planets for local target simulation. `--dummy` takes precedence if combined with `--target`.


## Mapping library prototype

The `map` module computes NewtonWars planet acceleration and its first and second spatial derivatives. The `simulation` module reproduces the source's 25-substep velocity-Verlet path and exposes position and reconstructed-acceleration residuals. `estimator::fit_greedy_source_counts` uses a deterministic grid search over acceleration residuals to seed planet candidates, then fits centers and gravity weights against shot paths. The separate `estimate` binary reads one recorded capture, extracts completed shot paths, and reports each candidate count's training and held-out position RMSE:

```sh
cargo run --bin estimate -- --capture-id 2
```

Use `--db-path PATH` for another SQLite file, `--max-sources N` to search further (the default is 4), `--validation-every 0` to disable the deterministic holdout, and `--radius-prior VALUE` / `--grid-spacing VALUE` to adjust seed assumptions. The default radius is fixed during fitting: smooth trajectories constrain centers and gravity weights, while radius needs collision evidence. Covariance is a local fit approximation and is reported only for full-rank fits. Candidate maps are proposals, not a claim that one count is correct. A local 61-shot capture favored two sources among the first four held-out scores; larger candidates had lower training error but much worse validation scores, demonstrating the risk of overfitting and local minima. The finite-difference fitter becomes expensive as source count grows, so raise the limit deliberately.

While fitting, the command shows a spinner because the nonlinear optimizer does not expose meaningful iteration progress. `train_position_rmse` is the root mean square error for each recorded x/y coordinate, in world units. Lower is better, especially on held-out shots. `converged=true` only means the optimizer met its stopping condition; a full-rank Jacobian only means its local parameters were numerically distinguishable at that candidate. Neither proves the planet count or map is correct. For traces recorded with `sendplanets 1`, the command automatically reports fit quality by each captured map epoch and compares candidate fits with the true map as a local diagnostic:

```sh
cargo run --bin estimate -- --capture-id 7
```

When oracle data are present, they define map epochs and provide a true-map score; the candidate fit still uses only shot paths. Each shot includes its launch point, so shots from before and after a hit can contribute to the same map fit. The diagnostic groups oracle scores by each shot's starting coordinate rather than event order, since a shot's completion can arrive after a later player-position update. Use `--no-oracle` to ignore available planet messages.

## Local NewtonWars server

The client does not start a server. From the NewtonWars checkout, the macOS source build is:

```sh
cd ../NewtonWars
./build-mac.sh
./nw fullscreen 0 sendplanets 1 throttle 16
```

The `sendplanets 1` option is required if the server should disclose planet positions. If a local server is already running, connect to it without starting another copy. Local NewtonWars connections may be run side by side; use `cargo run -- --clients 3 --target` to launch three parameter variants under one manager.

With recording enabled, the manager writes one capture for the whole run. The first connection writes shared world events once; every connection writes its own identity/energy events and outbound commands tagged by bot name. Older databases are upgraded by adding a nullable `bot_name` column; single-client events remain untagged as before.


## Offline learning scenarios

Run `cargo run --bin learning_scenarios` to compare flat, learned, and oracle-map targeting on deterministic synthetic worlds. The ladder covers flat controls, single-planet flybys and contacts, repeated contacts, partly learned two-planet fields, and established multi-planet shots. Each training path is checked for its intended termination; each target has a collision-checked witness. Reports include actual shot outcome, held-out path error, inferred planet error, and fit/search time. The command also writes inspectable SQLite captures to `learning-scenarios.sqlite3`, once per scenario and clean git revision. Open them with `cargo run --bin map_viewer -- --db-path learning-scenarios.sqlite3`. Use `--db-path PATH` to choose another capture database or `--no-captures` to print results without recording. Captures from uncommitted source are skipped so each recorded version has an unambiguous git revision.

For a causal replay of an existing SQLite capture, run `cargo run --bin capture_learning_scenarios -- --capture-id 29 --checkpoints 0,1,4,8 --holdout 2`. It fits only completed shots at each checkpoint and scores shots launched afterward. `--benchmark-targeting` also compares the physical-only and full beliefs on one reconstructed checkpoint. These are synchronous offline replays, not the beliefs actually published during the recorded game.

The oracle generates and scores synthetic paths; it is never passed to the online estimator. These cases share the bot's physics simulator, so they diagnose learning and targeting separately but cannot independently validate the simulator against the server. A single successful impact fixture does not imply every first shot identifies a planet.

The scenario tools are not yet an autonomous code-optimization loop. That planned loop needs engine-checked fixtures, a fixed multi-seed scorecard, and a machine-readable result for each candidate revision. It should make one focused code change at a time, score it against frozen selection cases, and confirm any apparent improvement on untouched cases before keeping it. Core gates should include held-out trajectory error, planet/contact recovery, targeting outcomes and energy, and fitter latency/map age; a change should not win by improving one metric while breaking the others. The report set and thresholds must stay fixed while candidates are tuned. Existing synthetic scenarios currently test estimator/simulator consistency; they do not establish engine parity or real-game hit rate.

## Targeting capture diagnostics

Analyze recorded paths without changing the database or running the expensive fitter:

```sh
cargo run --bin targeting_report -- --capture-id 21
cargo run --bin targeting_report -- --capture-id 21 --compare-policy
cargo run --bin targeting_report -- --capture-id 20 --fit-planets
```

The report separates map epochs and prints exact repeats, spatial overlap, short paths, and oracle trajectory RMSE when disclosed planets exist. Coverage measures distinct cells crossed by segments, excluding the launch cell and untimed final observation. All completed paths include debris. `--compare-policy` compares the coverage penalty against zero on the same recorded histories using oracle maps strictly for diagnostics. It reports overlap and nominal miss distance; it does not simulate new observations, reproduce live fitting, or establish hit rates. Bot angle profiles are assigned in first-identity order, which may differ from actual connection order.

`--fit-planets` replays observations sequentially and compares each new trajectory with the prior estimated map and a flat field. `--batch-shots N` is a diagnostic experiment that fits after N new trajectories; live estimation still fits sequentially. Batching can change associations as well as reduce computation, and comparisons must account for shots that were scored against different prior maps. Disclosed planet maps score the results and mark epochs only; they are never estimator inputs. These predictions are diagnostics, not hit rates.

## Local map and targeting inspector

Run a read-only local viewer over the recorded captures:

```sh
cargo run --bin map_viewer -- --db-path einsteinbot.sqlite3
```

Then visit `http://127.0.0.1:8765`. The viewer binds only to loopback and opens the SQLite database read-only. The page script is `web/map_viewer.js`, served by the binary alongside `web/map_viewer.html`.

**Captures.** The page opens on a table of every capture in the database (live runs and synthetic scenarios), sortable by any column, with the final decision's estimated planets, disclosed planets, missing planets, median center error, fit RMSE and convergence, plus global metrics over the rows shown (totals and medians; there is deliberately no single combined score). Error columns need a disclosed map. Click a row to open that capture at its latest state; the Captures button returns to the table. Only each capture's last recorded decision (highest sequence across bots) is read to build it.

**Time.** One scrubber spans every event and targeting decision in the selected capture. Decision ticks are colored by bot, shot ends and map resets have their own marks, and a trend strip above them plots the estimated planet count and fit RMSE at each decision (filled dots mark converged fits). Click or drag to scrub; ←/→ step events, Shift+←/→ step decisions (limited to the selected bot), Home/End jump to the first event or the latest state. A decision shows the belief published at that decision; any other event shows the latest decision snapshot recorded by then, together with the event history through that point. Captures save snapshots at decisions, not each intermediate fitter publication. Player positions and shots in flight come from the event stream up to the selected point, so they are current between decisions; the belief is only as recent as its snapshot, and paths that ended after it are drawn dashed and counted in the label. The viewer reads recorded state and evaluates it with the library; it does not refit the estimator.

**Layers.** Choose Gravity (the bot's belief), Uncertainty (propagated from the fit covariance; unavailable without a full-rank fit), Evidence (where recorded paths support a local residual) or Error vs disclosed (belief minus the game's own map; enabled only when the capture contains a `Planets` message). “Local corrections” toggles whether Gravity and Error include the observed residual correction or show the fitted planets alone. Overlays are estimated planets, disclosed planets (only with a disclosed map; colored close / center off / size off / missing), trajectories, contacts and an opt-in “Path residuals” dot overlay (observed path curvature minus the fitted planets' gravity, a diagnostic that can include numerical noise); players are always drawn, and a pink diamond marks the field-only source hypothesis when one exists. Colors span the 5th–95th percentile of the visible samples at 3-unit spacing (coarser automatically for full-map requests). Use the zoom buttons, Shift-drag to pan and double-click to reset.

**Inspection.** Hover for a summary of the object under the cursor; click a planet, player, contact, trajectory or empty point to pin it in the Selection panel. Only drawn layers can be selected. The sidebar ranks disclosed planets by how badly they are matched (missing first) and lists every estimated planet with its radius, mass, surface-contact count, position σ (unavailable without a fit covariance) and, with a disclosed map, its distance to the disclosed planet it is matched with. Grid spacing is only a display sampling interval; the physical gravity model is evaluated continuously.

**Shot review.** Selecting a decision re-simulates its chosen shot under the recorded belief and lines it up with the shot that actually followed (the first later shot by the same player with the same velocity and angle). The panel lists the observed path and each model's path (fitted planets only, planets plus residual correction, planets plus the field-only source when one was recorded, and the disclosed map when there is one) with RMSE over the shared timed samples, the first sample where each leaves the observed path by more than 10 units, and how each ends; ★ marks the model the bot used, and checkboxes toggle the paths on the map. The observed outcome (ended at a player, on a planet surface, left the battlefield) is inferred from the endpoint, not reported by the server. With a disclosed map, “Test repairs” changes one planet of the belief to the disclosed one, re-simulates, and ranks the changes by how much the path improves: a model sensitivity, not proof of cause, because the recorded residual correction stays in place. The green line in the timeline strip scores each decision's belief against the next five observed shots (median path error), which needs no disclosed map; “Worst predicted” jumps to the decision with the highest value. Beliefs are evaluated as recorded; the viewer does not refit the estimator.

The shot controls run the Rust simulator from the decision's captured launch and player state. When disclosed planets are available, the comparison button replays the recorded shot against them and reruns the current targeting policy on that map and captured bot state. This is a counterfactual using today's code, not the policy version that made the recorded choice. Captures without a `Planets` message cannot provide it. The viewer displays up to 60 recent completed paths through the selected point in the capture timeline.

Rust's gravity equation, velocity-Verlet step, 25 substeps per tick, launch direction, and sampled paths follow the NewtonWars source. We have not established bit-for-bit engine parity with a lockstep test. The viewer's timed-path audit on captures 41 and 42 matched 122 and 141 shots respectively; radial position-error p95 was 0.0069 and 0.0166 world units. The largest individual errors were 13.46 and 3.46 units, so inspect the worst-path overlay rather than treating the aggregate as proof. The audit uses the exact disclosed map for the selected epoch and excludes terminal samples whose substep time is not in the protocol. Player collisions and moving targets remain separate targeting concerns.
