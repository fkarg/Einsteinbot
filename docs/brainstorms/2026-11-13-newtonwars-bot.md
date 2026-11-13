# NewtonWars bot — grill-me capture

## Goal and scope
Build an information-gathering NewtonWars bot in Rust. The supplied Notes.md prioritizes discovering planet locations/gravity efficiently, tracking uncertainty, incrementally updating a map, and improving shot targeting; full RL is explicitly later. User suggests first determining local build/run, then a basic telnet client and communication map, keeping modules clean. Test coverage can remain small, with a few core tests.

## Architecture summary
Not settled. Candidate layers: transport/session, wire protocol, game state/map estimation, shot simulation/targeting, bot policy, executable/configuration. Actual boundaries depend on the upstream bot protocol and whether the server is modified/launched locally.

## Mode and checkout policy
Sequential grill-me mode. Default to current local checkout per user global AGENTS.md; no worktree unless concurrency constraint changes. Root checkout: `/Users/felix/Coding/Einsteinbot`; reference checkout: `../NewtonWars` (read-only unless asked otherwise).

## Evidence and recommendations
- Observed: Einsteinbot README describes an information-perfect NewtonWars bot; `Notes.md` contains map inference, uncertainty, incremental state, simulation, targeting priorities, and defers RL.
- Observed: NewtonWars is a C game played over telnet; it contains four bot examples (`bots/`). See upstream README and `network.h`.
- Recommendation pending protocol exploration: keep a basic protocol client useful as a standalone diagnostic executable, then put game understanding and policy behind independent Rust modules.

## Decisions / question backlog
- Q1: Core runtime shape (standalone telnet diagnostic first vs integrated bot client vs library-first). Ready; needs protocol exploration.
- Q2: Operational first milestone (local server/player setup and harness). Blocked on build/run exploration.
- Q3: Bot strategy scope for first useful bot (observe/map only vs also choose shots). Blocked on Q1 and protocol mapping.
- Q4: State estimation/model boundaries and uncertainty representation. Deferred until wire observations and game semantics are mapped.

## Rounds
### Round 1 preparation (2026-11-13)
Relevant facts: see Evidence above. Independent protocol/build exploration and current Rust documentation research are in progress.
Session assessment before peer: likely make the first Rust program a diagnostic telnet client with parser/state modules that can later host bot policy; advantage is observing real traffic and isolating protocol behavior, cost is some scaffolding before useful shots. Other materially distinct options: implement the bot directly against the existing server protocol (less intermediate tooling, but tighter coupling); make a reusable library first (strong modularity, weaker end-to-end feedback).
Peer result: pending.
User answers: pending.

## Implementation proposals
None ready before protocol/build facts and Round 1 decisions.

## Open flags
- Owner: main session. Determine exact NewtonWars build/run path and protocol/version handshake.
- Owner: user. Resolve architectural choices through interview; no implementation kickoff accepted yet.

## Maintained docs to update
README and a concise protocol/architecture document after behavior is implemented and verified.

### Round 1 answers and checkpoint (2026-11-13)
- Q1 settled by user: start with a Rust diagnostic telnet client; keep it cleanly separate from later bot input/control logic.
- Q2 settled by user: build and run NewtonWars locally first (macOS path included); existing server on another network is a later real test, and switching networks effectively means offline testing. Do not connect to it during local exploration.
- Q3 settled by user: prioritize observability, but generate activity through random actions paced by energy recovery so trajectories exist; improve policy later/soon.
- New requirement: use a small SQLite DB to track all communication, indexed per game, for deterministic reconstruction/debugging.
- User instruction: avoid peer-review for now; user expects direct fallback assessment. No peer call will be made for later rounds unless user changes this.
- Implementation acceptance: none. No code changes authorized yet beyond research/capture.

## Updated question backlog
- Q1: first Rust program/runtime shape — settled by user (diagnostic client first; separate future control logic).
- Q2: first operational milestone — settled by user (local build/run on macOS; external server later).
- Q3: initial bot behavior — settled by user (observability plus random energy-paced actions).
- Q4: SQLite event record contract: store exact raw wire bytes and directions/timing vs parsed semantic events only vs both. Ready; determines reconstruction fidelity and schema.
- Q5: session/game identity boundary and handling reconnect/partial sessions. Blocked on Q4 and upstream lifecycle details.
- Q6: local game run/build mechanics and reproducible macOS setup. Investigation active; dependent implementation proposals wait for findings.
- Q7: random action policy details. Deferred until energy message semantics/recovery and protocol actions are mapped.

## Round 2 preparation
Session assessment before user answers: for deterministic replay, keep raw inbound/outbound bytes in an ordered per-session event log, and add parsed event fields as a queryable projection; store a monotonic sequence number, direction, and receive/send timestamp. Avoid relying on timestamps alone for order. A versioned schema and game/session identifier are needed. Whether each game's log should be one DB transaction or append commits is an operational tradeoff to investigate.
Peer: intentionally skipped per user's instruction; direct session assessment only.
User answers: pending.

## Local build/run investigation
Pending; delegated read-only/source and safe local macOS build/run investigation. Do not write generated artifacts into `../NewtonWars` or connect to the network-hosted real server.

## Updated facts after local investigation
- Observed via bounded subagent inspection/build: on this Apple Silicon macOS host, `build-mac.sh`'s compiler/link flags work when source paths are rooted at `../NewtonWars` and output is directed to `/private/tmp/newtonwars-nw`. The generated binary was Mach-O arm64 and `--help` exited 0 without starting networking. `/opt/X11/lib` is absent but its warning was harmless in that compile; display.c has an existing `sprintf` overflow warning.
- The provided `build-mac.sh` assumes current directory is the NewtonWars root and writes `nw` there. Keep the reference checkout clean by invoking an equivalent command from Einsteinbot with `-o /private/tmp/newtonwars-nw`.
- Not observed: full game launch or client session. `main.c` initializes the network listener before display and then enters the game loop; launching can occupy local port 3490, so don't do it until checking the local port and the user is ready to run it.
- Observed source details: server sends own energy only to bot protocol >= 9; protocol >= 5 gets shot-begin and >= 6 gets shot-finish; version 0 defaults to 4. Game mode is included only versions 7-8 (and condition expression is version >=7 && <=8). Protocol values are native-endian words, and C's `send` call isn't a complete-write loop; Rust must handle TCP partial reads/writes.
- Existing checkout state observed before this investigation: user/pre-existing uncommitted changes include `.gitignore`, `README.md`, untracked `AGENTS.md`, `Cargo.toml`, `Notes.md`, `src/main.rs`, and docs. Do not overwrite or attribute these to this work. Current investigation added only this brainstorm file. The subagent reported its reference checkout status stayed clean.

## Round 1 peer disposition
Per user's instruction, no cross-model peer was used. Earlier peer attempt failed because the peer CLI's Claude OAuth token was revoked; no result was used. A fresh-context same-model fallback was used before the user explicitly instructed to refrain from peer, and it independently agreed with the session recommendations; its dissent/caveat was that passive observation may be insufficient and a small experiment mode may be needed.

## Round 2 assessment before asking
- Q4 options: (A) parsed events only — simple, queryable, but cannot reproduce decoding or recover parser bugs; (B) raw bytes only — faithful but awkward to query; (C) raw inbound/outbound payloads plus parsed event projection — supports byte-faithful replay and useful SQL queries, with modest duplication/schema work.
- Recommendation: C, with an ordered per-connection log, direction, monotonic sequence number, raw payload, and parsed record where decoding succeeds. Add wall-clock time for correlation, but do not make it the replay order. Durable commit batching remains a tunable operational question, not needed for the first schema decision.
- Peer: intentionally skipped per user instruction.
- Follow-up launch attempt: running `/private/tmp/newtonwars-nw fullscreen 0 sendplanets 1 throttle 16` twice printed `waiting for connections...` on the first run and then exited with `Swift/SwiftNativeNSArray.swift:78: Fatal error: Array index out of range`; second run reproduced the Swift fatal immediately. The server did not remain listening on port 3490. `lldb --batch ...` did not launch a process (`process exited with status -1`), so no native backtrace was obtained. Cause is unresolved; likely the failure is in GUI/runtime initialization after listener setup, but that is only an inference. No upstream code was changed.
- Local operation milestone is therefore partially complete: compile and `--help` work; game loop/window launch does not currently work on this host. Open flag remains for investigating host GUI/GLUT runtime and a safe local simulation path.

### Round 2 answers and checkpoint (2026-11-13)
- Q4 settled by user: SQLite stores fully parsed semantic events only; the project does not need raw underlying TCP byte capture. Preserve event order and timestamps because the bot's available information and decisions are what matter.
- New data-boundary requirement: record the information delivered to bot logic and its inputs/outputs, not lower-level transport bytes.
- Recorder policy: only one bot instance per game records to avoid duplicate data from multiple perspectives. User explicitly delegates the mechanism choice; an opt-in CLI recording flag is acceptable. Recommended simplest mechanism: designate one process with `--record` (or equivalent) and keep other bot processes non-recording; do not add stale-PID arbitration unless evidence shows operators need automatic election. Need confirm whether per-bot own-ID/energy events make perspectives non-identical; if so the designated bot's perspective is the canonical logged stream.
- User reports manually starting via `./nm` works, and says the game is currently running locally if needed. This supersedes the earlier attempted launch result as the current usable run path; the failure cause for the temporary binary invocation remains unknown and is not treated as a game-wide launch failure.
- Peer remains unavailable by user instruction; no peer call will be made.
- Existing uncommitted project files remain user/pre-existing and must be preserved. This capture is the only current change authored in this workstream.

## Updated question backlog
- Q4: log raw transport vs parsed events — settled by user (parsed events only).
- Q5: identify one canonical recorder among multiple bot instances — resolved by user policy (one per game); implementation mechanism delegated to main session; prefer explicit CLI opt-in, no automatic liveness election unless required.
- Q6: macOS build/run route — locally running game is reported by user via `./nm`; determine where `nm` comes from and document repeatable operation without taking over user's process.
- Q7: diagnostic client contracts and telemetry event schema — investigation active; distinguish protocol parsing, semantic observations, bot input/output, and persisted event rows.
- Q8: random action cadence/energy policy — deferred until protocol events and energy recovery are fully mapped.
- Path clarification needed: at inspection time, `../NewtonWars/nm` did not exist. User's report that `./nm` runs is recorded as their observation, but its working directory/binary path is not established; do not substitute another launch method until clarified.

### Round 3 answers and checkpoint (2026-11-13)
- Q6 path correction: user meant `../NewtonWars/nw` (not `nm`). This path existed in the initial checkout inventory; user reports it runs locally. The earlier discrepancy is resolved. Do not infer the temporary binary's crash applies to the checked-in `nw` executable.
- New requirement: bot name defaults to a random choice from a curated list of very well-known scientists (examples include Einstein), explicitly excluding Newton. A CLI, environment, or config value overrides the random default. Proposed conventional precedence if multiple overrides are set: CLI > environment > config; record as an implementation assumption unless user corrects it.
- User explicitly assigns ownership to this session for editing and committing `src/`, `Cargo.toml`, `Cargo.lock`, `AGENTS.md`, and `README.md` going forward. `Notes.md` remains user-owned and must not be edited. Existing changes in those delegated files may now be edited, but preserve intent and inspect before changing.
- User remains explicit that peer-review is unavailable; no peer call will be made.
- Recorder implementation remains delegated to main session; recommendation is an explicit `--record` opt-in for one designated bot process, with no automatic PID liveness election.
- Session mode remains sequential. Checkpoint recorded before further design/implementation work.

## Updated backlog
- Q6 local game path — resolved by correction: `../NewtonWars/nw`; user reports it is running.
- Q9 bot display name — resolved: random famous scientist name excluding Newton only when no supplied CLI/env/config override. Conventional override precedence is an implementation assumption.
- Q10 exact first-step scope — pending architecture/protocol map; the broad goal is accepted, but the grill-me process still needs to present a concrete P proposal with files, interface, invariants, behavioral coverage, docs, and commit scope before implementation.

### Round 4 answers and checkpoint (2026-11-13)
- User confirms `../NewtonWars/nw` is already running and explicitly authorizes live testing the Rust telnet client against it. Do not start a second server. Live testing should target the local instance only; the remote competitor environment remains deferred.
- User requests using existing libraries where available for telnet/network client, SQLite, and later mathematical work. Research maintained Rust crates and official docs before choosing; no library choice is settled yet.
- File ownership and `Notes.md` restriction from Round 3 remain in force.
- Peer-review remains unavailable and is intentionally not called.
- Implementation acceptance: user authorized editing/committing project files and live-testing the client, but no proposal with exact module/schema scope has yet been presented. Prepare a bounded P1 proposal after research; do not treat the research request itself as acceptance.

## New research
- Maintainer-doc research on rusqlite: pending broader library comparison.

## Updated open questions
- Q11: define a `game_id` boundary for persisted parsed events. Source protocol has no explicit game ID and server loop is continuous; candidate boundaries are one recorder TCP connection (simple, reconnect creates a new capture) vs operator-supplied game ID grouping reconnects. Session recommendation pending inspection of server lifecycle and user focus.
- Q12: suitable library set for current telnet/protocol diagnostics and future math — research active.

## Protocol map (source inspection, 2026-11-13)
- `network.c`: TCP port 3490; new peers receive a text welcome prompt; `n <name>` renames; `b <version>` enters bot protocol. Version 0 maps to protocol 4. To avoid legacy behavior and obtain `MSG_GAMEMODE` plus on-demand energy, target protocol 8: versions 7–8 receive game mode, versions >=8 can request own energy with `u`; shot-begin is >=5 and shot-finish is >=6. Correction to earlier note: own energy is not pushed continuously; a client must request it with `u`, and this is supported at version >=8.
- Incoming event IDs/frames: 1 own ID (type,id); 2 player leave (type,id); 3 player position (type,id,f32 x,f32 y); 4 legacy shot finished (type,id,u32 point_count, points as f32 pairs); 5 shot begin (type,id,f64 angle,f64 velocity); 6 shot finish (type,id,f64 angle,f64 velocity,u32 point_count, points as f32 pairs); 7 game mode (type,id,u32 mode); 8 own energy response (type,id,f64 energy); 9 planet positions (type,count,sizeof(SimPlanet), then each planet's f64 x/y/radius/mass).
- `sendPlanets` sends planets during bot negotiation and on reinitialize only if server setting `sendplanets 1` is enabled. Its `SimPlanet` is 32 bytes (two doubles position + radius + mass); fields are copied into the 32-byte record at offsets 0/8/16/24.
- Bot commands are text lines: `n name`, `b 8`, `v velocity`, numeric angle line, `u` request energy, and `q` disconnect. Existing bot example sends velocity then angle.
- Server runs continuously; no game identifier appears in this protocol. It initializes simulation once and loops forever. A connection therefore gives a reliable capture start/stop boundary, but the wire stream cannot automatically group a reconnect with its earlier connection.

## Library research (maintainer docs checked 2026-11)
- Rust `TcpStream` has the necessary synchronous TCP operations; no Telnet option negotiation exists in NewtonWars source. The `telnet` crate implements Telnet control negotiation; avoid it because this is custom mixed text/binary TCP and Telnet control-byte handling would be extra/conflicting machinery. Use std `TcpStream` with a local NewtonWars framing parser: https://doc.rust-lang.org/std/net/struct.TcpStream.html and https://docs.rs/telnet/latest/telnet/.
- `rusqlite` is synchronous and its `bundled` feature avoids system SQLite discovery/version drift at compile-time cost and requires a C toolchain. Recommendation: use rusqlite + bundled for the local application: https://docs.rs/crate/rusqlite/latest.
- Math recommendation: defer numerical dependencies until model/observations are designed. `nalgebra` is a matrix library; `adskalman` is linear Kalman/RTS with caller-side linearization needed for nonlinear observation models. Avoid selecting an estimator crate before modeling: https://docs.rs/nalgebra/latest/nalgebra/, https://docs.rs/adskalman/latest/adskalman/, https://docs.rs/kalman_filters/latest/kalman_filters/extended/.
- Random names: `rand` supports choosing from a curated slice via `IndexedRandom::choose`; pin the crate and use its current API: https://docs.rs/rand/latest/rand/seq/trait.IndexedRandom.html.
- Research source: delegated web research agent, with findings reviewed in main session. No cross-model peer used.

## P1 proposal draft (not yet accepted)
Build the first Rust diagnostic client in this checkout. Modules: CLI/configuration and name selection; `protocol` typed messages + incremental parser; `client` TCP session and bot-protocol startup; `store` SQLite event log; `main` wiring. Defaults: `127.0.0.1:3490`, bot protocol 8, random curated scientist name (never Newton) unless CLI/env/config supplies it. Use `std::net::TcpStream`, `rand`, TOML config, `rusqlite` with `bundled`, and serde serialization for semantic events. An explicit `--record` flag enables one designated process to write; a capture/game record is created per recording TCP connection, with PID, start/end timestamps, settings and ordered events. Log parsed inbound game events and semantic outbound bot commands; sequence is authoritative, timestamps contextual. No raw TCP bytes. No autonomous shots in P1; random energy-paced action policy is a later isolated module/step. Core tests: feed fragmented/multiple event frames (including variable-size shot paths/planet records) through parser; verify SQLite preserves semantic event values and per-game sequence. Update README for macOS build/run/client usage and debug the temporary-binary crash separately if it matters.
- Scope/review condition: peer calls remain disabled per user; do not modify upstream `../NewtonWars`; live test only against user's existing local `../NewtonWars/nw` listener, disconnect cleanly.

### Round 5 answers and checkpoint (2026-11-13)
- Q11 settled by user: one capture per recording connection.
- P1 accepted by user as previously scoped, with the following steering: initially model the CLI as spawning/running one bot; user raised a possible configurable client count and local multi-bot management with trivial actions, and notes this could centralize logging. This creates a new architectural decision before defining CLI/client ownership. P1 accepted behavior that does not depend on multi-client orchestration (protocol/parser/event store) remains in scope; orchestration and live client tests wait for Q12.
- User authorized live testing against already-running `../NewtonWars/nw`; observed in this session with `lsof`: a process named `nw` listens on TCP port 3490. Do not start another server.
- Source evidence for per-IP restriction: `network.c` handles connections as `local ? 0 : is_connected(remoteIP)` and exempts localhost addresses from duplicate-IP rejection. This matches the user's statement that the actual server accepts only one connection per non-local remote IP; localhost permits multiple connections.
- Library research: use `TcpStream`, `rusqlite` bundled, and defer math dependencies. See source links in Library research.

## Q12 — multi-client ownership (ready)
Context: P1 has a diagnostic TCP client and semantic event DB; future strategy logic should remain separate. Upstream NewtonWars rejects multiple simultaneous connections from one non-local remote IP but permits multiple local connections. User wants to defer multi-bot complexity but raised one CLI with a configurable client count/local management and trivial behavior as a way to simplify logging coordination.
Options prepared: (A) one bot connection per CLI invocation for P1; run several local invocations manually, designate one `--record`; simplest diagnostic boundary but operator coordinates instances. (B) one long-running CLI process manages N client sessions; central config/log ownership, but concurrency and event attribution arrive in P1. (C) one CLI supervisor spawns N independent bot processes; process isolation and centralized lifecycle, but needs child management and shared logging policy. Session recommendation: A for this protocol-first P1 and defer count/supervision until the basic client is understood; the user's preference may favor B/C.
Peer: intentionally not called because user says peer remains unavailable. No peer options are presented.
User answer: pending.

## P1 accepted scope checkpoint
Accepted, pending Q12 for the process/client ownership boundary. Stable work: module boundaries, TCP/protocol parser, typed semantic events, SQLite connection-scoped event store, name/config selection, core tests, README usage. Client fan-out and live multi-client behavior are held until Q12. No implementation edits have started.

### P1 foundation implementation checkpoint (2026-11-13)
- Implemented stable foundation only: `src/protocol.rs` typed server events and semantic client commands, incremental native-endian parser for message IDs 1–9, `src/event_store.rs` SQLite event capture with ordered typed JSON events, per-connection capture metadata and explicit end timestamp, and library exports in `src/lib.rs`. Added rusqlite bundled + serde dependencies and normalized Cargo package to `einsteinbot`.
- Behavior contracts: parser accepts arbitrary read fragmentation and concatenated events; variable shot/planet counts are bounded (2,000 points / 64 planets); planet record size must be 32 bytes; unknown IDs and malformed framing return errors. Persisted data contains semantic event JSON only, capture-local sequence is authoritative, Unix-millisecond timestamps are contextual. `finish()` stores connection end time; an unfinalized capture remains NULL and can signal abrupt termination.
- Core test coverage now includes fragmented fixed and variable frames, multiple events in a read, variable shot/planet decoding, text command encoding/validation, semantic event sequence round-trip, and separation/end-time across two captures in one DB.
- Verification observed: `CARGO_HOME=/private/tmp/einsteinbot-cargo cargo test` passed (5 tests); `cargo fmt --check` passed; package-name warning removed.
- General fresh-context same-model review found no blocking protocol/storage defects; its note that timestamps can collide within one millisecond is handled by sequence ordering. Test-quality review identified variable-length fragmentation and capture isolation as worthwhile coverage; both were added. Impact: added verification.
- Files intentionally not yet implemented: `src/main.rs` remains Hello World; no TCP connection/prompt handling, CLI/name/config, logger wiring, bot actions, README operation instructions, or live telnet test yet.
- P1 remains accepted but incomplete; single-vs-multiple client process/connection ownership is held at Q12. Stable code slice is ready to commit independently.

### Round 6 answers and checkpoint (2026-11-13)
- Q12 settled by user: option A. One TCP connection per CLI invocation for now; user will start `demo-dummies` manually when target players are needed. No client-count option, multi-client manager, child-process supervisor, or bot firing policy in P1.
- P1 accepted and now unblocked: wire one diagnostic client to the committed parser/store, one connection per invocation, optional `--record` on the designated process. User authorizes localhost live testing against the running `../NewtonWars/nw`; do not start a second server. No actual client connection has yet been made.
- File ownership from Round 3 remains: this session may edit/commit Rust project files and README/AGENTS; do not edit `Notes.md`.
- Library decisions from current maintainer docs: standard `TcpStream` for custom mixed text/binary protocol (no Telnet negotiation); `rusqlite` bundled; `rand` 0.10 for curated name sampling; `toml` serde deserialization for optional config; defer math dependencies pending an estimator model.
- Name precedence is config < environment < CLI; no value selects uniformly from a curated list excluding Newton.

### Round 7 answers and checkpoint (2026-11-13)
- Logging policy revised by user: recording defaults ON; the CLI/config/environment can disable it with `--no-record` or a false record setting.
- New behavior requirement: add an opt-in dummy mode that fires in random directions with low, somewhat randomized shot cost, only as available energy permits. Keep it separate from diagnostic mode and later policy code. Main-session recommendation: on each requested own-energy response (currently polled once per second), choose an integer shot velocity/cost in [2, min(8, floor(energy))]; if energy < 2, skip. Send velocity then random integer angle in [-180,180]. At most one shot per energy response keeps it trivial and means low energy naturally suppresses firing; authoritative next energy response resets the estimate. User has not specified exact numeric range; this is a proposed simple default, not yet implemented.
- Source inspection for rate limits: `network.c:update_limits` refills one byte every third `stepNetwork` loop and caps each `connection[k].limit` at 512; accepted non-local `recv` bytes are subtracted from that connection's bucket; local addresses bypass this limit. This is per TCP connection, not one shared bucket per IP. With the game's expected ~60 loop/s, nominal refill is ~20 B/s, but the exact real-time rate changes with vsync/throttle. `conf.rate` is 5 energy/s in this checkout (despite stale help text saying 2.0); energy is increased per simulation step and a shot subtracts its velocity.
- Rate-limit implementation recommendation: enforce a strict app-side 16-byte-per-second cap per TCP connection (below the nominal 20 B/s) plus no more than the upstream 512-byte burst; document that upstream's refill is frame-paced and therefore its exact wall-clock rate varies. Cap applies to every client command, including dummy actions. The local server bypasses its own cap, but our client will still obey its cap.
- User's Q12 choice remains: one connection per CLI invocation; user runs demo-dummies manually. No client manager/count option.
- These requirements reopen client behavior beyond the initially accepted passive P1. The app implementation is still in progress; no live network test has happened yet.
- User follow-up on rate limits: preserve a later backlog item to account for per-connection byte usage/throttling in persisted telemetry; for this step, satisfy the constraint simply with the hard outbound cap and defer rate accounting/analytics.

### Round 8 requirements and implementation checkpoint (2026-11-13)
- User's recording policy: parsed-event recording defaults ON. CLI `--no-record`, `record=false` in TOML, or `EINSTEINBOT_RECORD=false` disable it.
- User added an opt-in dummy mode: low-ish randomized shot energy/velocity, random directions, cadence constrained by available energy. Implemented as `--dummy`/config/env boolean. On each own-energy response (requested at 1 Hz), it chooses one integer velocity in `[2, min(8, floor(energy))]` and one angle in `[-180,180]`; if energy < 2 it skips. Sends velocity then angle. One shot max per energy response. Passive remains default.
- User requested hard byte caps. Upstream source tracks a per-TCP-connection byte bucket, initialized at 512 and replenished by one byte every three `stepNetwork` calls; localhost is exempt. At nominal 60Hz this is ~20 B/s; actual wall-clock rate varies with loop pace/throttle. Client now enforces a 16 B/s token-bucket refill with at most 512 bytes burst on every outgoing command. Per-connection byte/throttle metrics are deferred by user for later; no rate accounting schema yet.
- Added `ctrlc` handler after live run showed SIGINT otherwise leaves `ended_at_unix_ms` NULL. Ctrl-C now causes semantic `q` disconnect and capture finalization.
- Console output now summarizes shot paths rather than printing every point; full parsed trajectories remain in SQLite.
- Live test attempt 1 failed with `unknown message type 467006`. Root cause observed in upstream `network.c:857-860`: while still text-mode, the server emits `> ` after the `n name` command. The client had entered binary parse before consuming it. Fixed by consuming the second prompt before sending `b 8`; added a focused prompt test. This is the bug that exposed the framing boundary.
- Live test attempt 2 against user's existing local listener succeeded. Observed `GameMode`, `OwnId`, `PlayerPosition`, `OwnEnergy`, `ShotBegin`, and `ShotFin`; dummy generated shots with integer velocities in range and random angles. SQLite at `/private/tmp/einsteinbot-live3.sqlite3` held 41 parsed events: 6 energy updates, 6 shot begins, 5 shot finishes, 6 each set-velocity/set-angle, 6 energy requests, setup commands, and disconnect. Capture had non-NULL end time after Ctrl-C. No planet event appeared in this run. The server remained listening afterward. No raw bytes were stored.
- Verification observed after all changes: `CARGO_HOME=/private/tmp/einsteinbot-cargo cargo test` passed 16 tests; `cargo fmt --check` passed; `cargo run -- --help` passed. The initial post-Ctrl-C edit failed only `cargo fmt --check`; `cargo fmt` corrected it and all checks passed on rerun.
- External library verification: `ctrlc` 3.5.2 `set_handler` uses a handler thread and atomic flag pattern; docs: https://docs.rs/ctrlc/latest/ctrlc/ .
- Fresh-context runtime review and test-quality review are pending before commit. User requested no peer-review.

### P1 implementation and review closure (2026-11-13)
- P1 is implemented: one CLI invocation opens one connection; text welcome and post-name prompts are consumed before bot mode; protocol 8 typed events are printed with compact shot summaries; energy is requested every second; default name is random curated scientist excluding Newton, with CLI/env/TOML precedence; names are validated as printable ASCII <=15 bytes; semantic event recording is on by default and `--no-record` disables it; `--dummy` adds one energy-gated low-cost random shot per energy response; Ctrl-C sends `q` and finalizes the per-connection capture.
- Outbound limiter is per TCP connection, 16 bytes/sec token refill and max512 burst. It governs all commands. User deferred byte-rate accounting/metrics; no such telemetry was added.
- General runtime review: no blocking defects. Findings: (P2) record semantic outbound command before socket write so failed/partial send intent remains reconstructable — fixed; (P3) validate against server's 15-byte name limit so config/log/server-visible name agree — fixed. Impact: added durability and boundary validation.
- Test-quality review: current tests cover defaults/precedence, name exclusion and length, two-prompt handshake, protocol fragmentation/variable records, dummy energy/angle bounds, rate limiter burst/refill, and SQLite event ordering/isolation. An end-to-end emulator was considered but not added; existing focused tests plus live run give sufficient evidence at this stage. Impact: no added work.
- Simplicity review: no changes recommended. Pre-mortem: no blocker; reminder that no Planets event appeared in the live session. `sendplanets 1` is required; current server was left running and untouched.
- Observed final verification: 17 tests passed; `cargo fmt --check` passed; `cargo run -- --help` passed. Local live passive run against existing `nw` received game mode, own ID, player position and energy; default recording created 18 semantic events and a finalized capture. Local live `--dummy` run received trajectory events, emitted six shots over the capture, stored complete parsed shot point lists, logged disconnect and finalized the capture. Both runs left `nw` listening. The rate limiter is unit-tested; localhost bypasses the server's own limit, so integration did not demonstrate server-side throttling.
- Reviewer impacts: general review fixed two concrete findings; test-quality review confirmed coverage, no decision impact beyond coverage audit; simplicity review had no decision impact; pre-mortem added the `sendplanets 1` operating reminder. All reviews were same-model in-session; user explicitly disallowed peer-review, so no cross-model review was performed.
- Durable operational guidance is now in README.md and AGENTS.md. `Notes.md` was not edited.

## Remaining workstream flags / next frontier
- Local owner: current `nw` emitted no `Planets` event. User can relaunch with `sendplanets 1` when ready; do not stop/reconfigure the running process.
- Main session: later add per-connection byte usage/throttle accounting to the persisted semantic log, as user requested eventually.
- Main session: map/estimation design remains open. Determine how to use optional direct planet messages locally while supporting inference from trajectories when absent; map physical observables and inverse problem before implementation.
- User: remote competitor environment remains a later target after local diagnostic and bot behavior are understood.
- Future: improve dummy policy and then design incremental map/uncertainty state; RL stays deferred.

## Next design round preparation — map representation (2026-11-13)

### Relevant local facts
- Observed in `../NewtonWars/simulation.c`: default game has 24 planets; each radius is sampled uniformly from 20–40 and mass is `radius^3 / 10`; positions are non-overlapping with radius-sized battlefield margins. Generation retries until a potential-grid condition succeeds.
- Observed: acceleration is a sum of source contributions `unit(planet_position - point) * mass / distance^2`; no extra gravitational constant. The simulation uses velocity-Verlet with 25 substeps per game-loop step. It logs each loop's shot position as f32; a completed `SHOTFIN` sends the full trajectory, so observations arrive at shot completion rather than continuously.
- Observed: `sendplanets 1` sends count, f64 position, radius and mass at bot handshake and after reinitialize; default is off. The live capture had no Planets event. Player start positions are selected using map potential, not uniform across the battlefield.
- Inference: `mass = radius^3/10` is a useful source-specific prior, but shouldn't be assumed for a different server build without checking direct events or residuals. Planet count/model ambiguity means a single fitted map may not be unique.

### External estimation research
- JPL and DESCANSO describe orbit/trajectory reconstruction using iterative weighted least squares around a nonlinear propagated model, repeatedly relinearized. This is a useful baseline for comparing a candidate map against full observed paths: https://cneos.jpl.nasa.gov/sentry/intro.html and https://descanso.jpl.nasa.gov/monograph/series1/Descanso1_all.pdf.
- Particle filters can retain nonlinear/multimodal uncertainty but require a candidate population and more computation: Gordon et al. (1993), https://ahstat.github.io/images/2014-7-11-Introduction-particle-filters/gordon1993_particle_filters.pdf.
- Identifiability work notes that distinct source configurations may produce the same observations; no optimizer can resolve them without informative trajectories or priors: https://pmc.ncbi.nlm.nih.gov/articles/PMC6685009/ and https://arxiv.org/abs/1004.4939.
- Design tension: batch fitting is an established baseline, while user Notes.md requests state updates whose cost does not grow with old trajectory count. Record this as a policy: keep online updates bounded with respect to the number of historical shots, likely via fixed-size sufficient state per candidate model; whether occasional offline refinement is allowed remains open.

### Q13 — representation for the first gravity/map estimate
Context: NewtonWars' source represents a finite set of planets with position, radius and mass, and shots reveal full trajectories only when finished. The target environment may not send direct planets; local runs can expose them with `sendplanets 1`. The Notes ask for planet count/location/size uncertainty and gravity effects, while also asking for updates that do not revisit all prior trajectories.
Options: (A) explicit planet hypotheses with uncertain count/position/radius/mass — matches source and goal and remains compact, but is nonlinear and can have multiple plausible layouts; (B) grid of gravity values with uncertainty — flexible and naturally map-shaped, but does not directly recover count/radius/mass and carries many cells; (C) hybrid planet hypotheses plus residual gravity grid — can explain mismatches but brings more state and complexity.
Session recommendation: A for the first model because the server physics is exactly a sum of planet contributions and Notes explicitly prioritize planet properties. Keep fixed-size online summaries per hypothesis; revisit grid residuals only if observed trajectories show the source model is inadequate. No peer call by explicit user instruction.
User answer: pending.

### Round 9 answer and checkpoint (2026-11-13)
- Q13 settled by user: use a hybrid representation (explicit planet hypotheses plus a residual gravity field) with uncertainty tracking.
- User asks whether to frame uncertainty in distribution/Kalman-filter terms or use another estimator. This is a new Q14: select the uncertainty/update framework for a hybrid, nonlinear, partly discrete map model. Prerequisite Q13 is settled; research on suitable filtering/online estimation is active.
- Session's assessment before further research: a single Gaussian Kalman state is likely too restrictive for uncertain source count and multiple plausible planet layouts. Tentatively favor a small distribution over planet hypotheses plus bounded incremental uncertainty/update state for the residual field; particle/ensemble methods may represent discrete alternatives, with higher computational and tuning cost. This is preliminary, not a settled choice.
- Respect user's bounded-history policy: update cost should not grow with number of past shots; per-hypothesis/grid state can remain fixed-size, though compute may grow with number of maintained hypotheses/grid cells.
- Peer review remains explicitly unavailable by user instruction. Do not call peer-review; use primary external research and local source evidence.

### Round 10 steering and checkpoint (2026-11-13)
- User reaffirmed Q13 as hybrid planet hypotheses + residual gravity field with uncertainty.
- User asks to explore the actual “perfect information” map-building process and whether Kalman/distribution framing is suitable. Treat Kalman as an exploration prompt, not a decision to use a particular filter.
- Next response should explain the end-to-end loop with observed inputs, forward physics, posterior/candidate updates, derived field uncertainty, and informative shot selection. Keep facts/inferences separate and identify where the next user choice is genuinely needed.
- Research on mixed discrete/continuous filter alternatives is active. No peer-review call by explicit user instruction.

### Round 10 follow-up and checkpoint (2026-11-13)
- User clarified that “Kalman-filter terms” means the broader filter family/concepts, not a request for a plain Kalman filter. User expects uncertainty/information scarcity and active information gathering to be central early, with accurate simulation becoming more important after enough data is collected.
- Session assessment revised: do not dismiss Kalman-family methods. A fixed-dimensional residual field may suit ensemble/unscented or information-form updates; discrete planet-count/layout alternatives may need multiple hypotheses or particles. An active experiment policy can use uncertainty reduction per energy early, then shift weight toward accurate forward simulation/targeting as posterior uncertainty falls. Exact method remains open pending research and design.
- Research question now spans hybrid estimation + active experiment design + the transition from exploration to exploitation. No peer call per user's instruction.

## Kalman-family and active-information research
- User clarifies Kalman filter is a broad conceptual frame: uncertainty distributions and active information gathering matter early; high-fidelity simulation matters more as knowledge accumulates. Do not treat the earlier concern about a plain single Gaussian Kalman filter as rejection of the family.
- Observed research evidence: EnKF has been used for nonlinear model-parameter estimation (Akita et al., J-STAGE: https://www.jstage.jst.go.jp/article/jsces/2010/0/2010_0_20100021/_article/-char/en). Particle filters represent nonlinear/non-Gaussian distributions as weighted samples but cost scales with particle count and can lose diversity (Gordon et al. 1993: https://people.bordeaux.inria.fr/pierre.delmoral/gordon-salmond-smith-1993.pdf). Unknown-count tracking with RBPF uses discrete hypotheses and conditional filters but requires birth/death model machinery (Särkkä et al.: https://users.aalto.fi/~ssarkka/pub/nrbmcda-preprint.pdf). Nonlinear Bayesian experimental design commonly values experiments by expected information gain, but computing exact EIG can be expensive (Zhong et al. 2025: https://doi.org/10.1137/24M1649344).
- Session recommendation, inferred from the methods' assumptions: maintain a small mixture of discrete map hypotheses (including count/layout alternatives), with a fixed-size ensemble/Kalman-style continuous estimate within each for planet parameters and residual-field coefficients. This preserves the user's Kalman-family intuition without averaging distinct maps into one implausible mean. Avoid a full high-dimensional particle filter until evidence requires it. Approximate information gain from the ensemble spread for candidate shots, divided by energy early; gradually increase simulation/targeting utility as posterior field uncertainty falls. This is a design hypothesis, not a settled implementation choice.
- Q14 prepared: when exact `Planets` data is present, should it feed gameplay state directly, serve only as an evaluation oracle, or be tracked separately from the inferred map? Session recommendation: store/use it as exact known state if the live server provides it, while retaining a separate inferred map for evaluation and for deployments where the event is absent. This permits exploiting available information without letting local oracle data hide inference defects.

### Round 11 answer and checkpoint (2026-11-13)
- Q14 settled: maintain two separate maps. Never depend on live planet messages outside local testing; when enabled locally, use the exact map for testing/target practice and comparison against the independently inferred map. This is an evaluation/test oracle, not a production input.
- New modeling direction from user: prioritize explicit planet estimates, the underlying gravity/acceleration field and its spatial derivatives, and uncertainty. Use these to fill gaps and choose targeting bets by expected payoff, with inverse/forward trajectory calculations and information updates. Planet count and gravity strength should emerge from fitting trajectory/field evidence rather than dominate the state design.
- User expects a planet to become nearly exactly estimable after two hits, including radius, while allowing overlapping planets. Treat this as a hypothesis to examine against the actual collision observations: a completed path ends at a sampled point inside a planet, so the endpoint alone does not expose the exact surface or center. Two distinct hit trajectories may constrain a center/radius strongly, but identifiability and discretization need verification before we encode that as an invariant.
- Priors policy: use source defaults and the previous connection's values as starting priors for the same likely server, then confirm/update them from observations. Keep provenance so defaults, previous-session values, direct local oracle data, and inferred values are distinguishable.
- Update strategy remains open: user is comfortable with a full recalculation pipeline initially; incremental updates are desirable later for bounded historical cost and Kalman-state-style uncertainty deltas. Do not force online-only updates into the first estimation baseline.
- New ready decision: define the map's field representation and the meaning/role of its derivatives; clarify what constitutes a “hit” and what two hit paths can identify. This precedes selecting the estimator and active-shot objective.
- Peer review remains explicitly unavailable by user instruction; no peer call made.

### Round 12 answer and checkpoint (2026-11-13)
- User clarifies the two-hit claim as a cheap heuristic: two planet hits should identify center and circumference, hence gravity weight, unless the first shot already constrains the planet to about 99% confidence. Keep this as an estimator shortcut/expectation to validate, not a certainty rule; preserve residual ambiguity and uncertainty in all cases.
- User confirms field scope: acceleration plus spatial derivatives, with uncertainty. They ask whether uncertainty should use the same spatial map as all fields or be differentiated by coordinate.
- Session assessment: use a shared coordinate domain/grid for field values and uncertainty, but do not use a single shared uncertainty value. Each coordinate needs its own uncertainty, and acceleration components can have covariance with each other and across nearby coordinates. Derivative uncertainty should be derived from the joint field uncertainty (or ensemble), not stored as unrelated confidence numbers. A compact ensemble over map hypotheses and field states may preserve these correlations without a dense covariance matrix.
- User's ambiguity policy is now explicit: never collapse ambiguity into a single certain answer; the expected rarity of ambiguity does not justify dropping it.
- Next decision: choose the first practical uncertainty representation and whether derivative fields are stored or derived. Recommend a shared acceleration-field grid with per-location vector covariance, with spatial correlations represented by a small ensemble; derive Jacobian/curvature and their uncertainty from each ensemble member. This remains a proposal, not an implementation acceptance.
- Peer review remains unavailable by user instruction; no peer call made. External research checked Gaussian-process derivative covariance and ensemble Kalman field uncertainty. Those methods support deriving derivative uncertainty from joint covariance/ensemble rather than assigning independent derivative confidence.

### Round 13 steering and checkpoint (2026-11-13)
- User proposes revisiting Q13: use planet parameters as the primary uncertain state. Derive acceleration, derivatives, and uncertainty from planet position/radius/gravity weight; use the source mass-radius law only when confirmed/plausible. Keep trajectory residuals as model checks and add a residual field only if repeated evidence warrants it. This may replace the default hybrid-with-independent-field model; Q13 is reopened pending this exploration.
- User emphasizes that state representation can remain simple while inference is hard; retain competing planet configurations when evidence supports ambiguity. Do not equate two hits with certainty: the shortcut is a strong estimate when the first hit has not already reached about 99% confidence, with uncertainty preserved.
- Observed in `simulation.c`: collision is checked at each of 25 integration substeps per loop, but ordinary trajectory points are appended once after all substeps. On collision, the server immediately appends the collision substep position as the terminal point. Thus a hit endpoint is a rounded f32 point inside a planet, but the preceding transmitted point can be as much as a loop of travel earlier; the line between them is not an exact surface bracket. It constrains candidate planet surfaces together with the simulated curved path and the known collision substep behavior.
- Estimator exploration requested: formulate how trajectory points and termination constrain candidate planet configurations; propagate source-parameter uncertainty into acceleration/Jacobian fields; define residual evidence for keeping, splitting, or adding planet hypotheses. An independent research follow-up is pending. No implementation proposal is accepted.
