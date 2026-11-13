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
