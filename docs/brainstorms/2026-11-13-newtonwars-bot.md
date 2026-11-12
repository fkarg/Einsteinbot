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
