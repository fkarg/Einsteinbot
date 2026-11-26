# Project instructions

## Project context

This repository is **Einsteinbot**, a Rust project to build an information-gathering bot for [NewtonWars](https://github.com/Draradech/NewtonWars). The NewtonWars source checkout is expected at `../NewtonWars` and is the reference for its build process, runtime behavior, and client protocol.

The project target is to infer planet locations and gravity effects efficiently, maintain an incrementally updated map with uncertainty, and use that model to improve shot selection. Reinforcement learning is explicitly a later possibility, not an initial requirement.

For estimator evaluation, prioritize gravity-field accuracy in explored areas; also assess whole-field and unexplored-area accuracy, planet collision geometry, uncertainty, and trajectory prediction separately. Targeting and strategy ultimately optimize shot outcomes and game strength, not the estimator's field score alone. Use oracle truth only for evaluation, never as fit input. See [`docs/estimator-evaluation.md`](docs/estimator-evaluation.md) for the proposed scorecard and measurement semantics.

## Development direction

- First understand how to build and run the local NewtonWars game.
- Build a basic Rust telnet/TCP client and map the communication protocol before concentrating on bot strategy.
- Keep transport, protocol parsing, game state/map estimation, simulation, targeting, and bot policy modular, with clear interfaces.
- Keep early automated coverage small and focused on core behavior.
- Bias toward action: build a small end-to-end slice before spending time refining the design.
- Test uncertain details cheaply with local source inspection, synthetic examples, stored captures, or the local oracle when available; ask only when a real architectural tradeoff cannot be resolved by a reversible experiment.
- Prefer implementing a reasonable baseline and learning from its behavior over extended speculative design. Keep unresolved ideas explicit, but do not let them block independent implementation work.

## Settled runtime contracts

- One bot session owns one TCP connection. `--clients N` starts several concurrent sessions only against loopback hosts; same-IP remote fan-out is rejected by NewtonWars. A manager run has one SQLite capture: the first session records shared world events once, and each session tags its own identity/energy events and outbound commands with its bot name.
- The client uses the NewtonWars protocol version 8. Keep transport, typed protocol events, and bot policy in separate modules.
- SQLite recording defaults on and stores parsed semantic inbound events and outbound commands in global sequence order per invocation. A multi-client manager records shared world events once and tags bot-specific events and commands; it does not store raw TCP bytes. `--no-record` disables it; `--dummy` opts into the simple energy-gated firing policy.
- A SQLite capture remains one connection's complete replay log. The estimator treats each observed `Planets` update as a separate map-fit episode; oracle maps are used automatically when present for boundaries and scoring, never as fit input. A shot's first recorded point is its launch position, so fit paths across player position changes together when the map is unchanged.
- The client rate limiter is per TCP connection: 16 outbound wire bytes per second with a 512-byte burst. Upstream refills one byte every three loop steps and exempts localhost, so its actual wall-clock rate depends on loop pace. Keep the client cap unless upstream source or limits change.
- NewtonWars only sends planet positions when launched with `sendplanets 1`.
- Targeting is opt-in and separate from transport. It prioritizes simulated hits; uncertainty probing is considered only without a predicted hit and above a threshold. The canonical connection feeds a background contact-seeded physical-planet estimator; all targeting sessions share its partial gravity map. Probable surface contacts seed planet geometry; radius, mass and position are refined jointly against trajectories and contact constraints. Ambiguous associations remain explicit and must not double-count gravity. Target mode uses an empty map until the first fit, unless local oracle-map use is explicitly enabled. Map resets clear shot history and invalidate older fit results; fitting is cooperatively cancelled on reset and shutdown. Targeting continues using the latest published belief while fitting proceeds. Live observations and fits remain sequential so contact associations see each preceding fit; finite-difference Jacobian columns are computed in parallel. The uncertainty score is a cheap marginal-variance exposure proxy, not posterior information gain.
- Targeting simulates against the physical planet map only. There is no residual gravity field and no field-only source hypothesis (removed 2026-11-26; git history is the only fallback, and a physical-planets-only v2 estimator is planned). Physical covariance remains explicitly unavailable where not identified.
- Targeting tracks per-map spatial coverage from completed trajectories. The coverage grid is a diversity heuristic; predicted hits retain priority, probes are discounted by novelty, and approach penalties are bounded. A possible planet contact alone must not veto or penalize a shot; simulation still accounts for planets in the current map. Missing covariance must remain explicitly unavailable, never presented as certainty.
- Do not penalize shots merely for being close in launch parameters to a previous shot: small adjustments can improve aim. Use actual observed trajectories and outcomes when refining a shot; keep predicted-hit priority and do not treat a possible planet impact as a categorical veto.
- Local manager variants use the same targeting policy with coarse angle steps of 8, 10, and 12 degrees and one-degree refinement of promising candidates; keep policy differences explicit and small until capture comparisons justify distinct roles. Search respects available energy and the server velocity limit, and prefers lower-energy predicted hits. Player collision predictions use latest known positions and physics substeps. Captures record per-decision state and build revision in metadata.
- `Notes.md` is user-owned; do not edit it. Durable project operation belongs in README.md and lasting development conventions belong here.

## Verification

- The Cargo `dev` profile uses `opt-level = 1` for Einsteinbot and `opt-level = 2` for dependencies; debug information and incremental compilation retain their defaults. Use the normal dev profile for local runs and scenario tools, `cargo check` for quick compile feedback, and `cargo test` for behavior checks. Use release builds only for optimized runtime and performance comparisons.
- Run `cargo fmt --check` and `cargo test` for Rust changes.
- Use the normal dev profile for `cargo run` commands unless measuring optimized performance; use `--release` for those comparisons.
- Live client checks may use an already-running local NewtonWars listener on port 3490. Do not start a second server if one is already running.

## Session state

See [`docs/brainstorms/2026-11-13-newtonwars-bot.md`](docs/brainstorms/2026-11-13-newtonwars-bot.md) for the active design/interview state, evidence, decisions, open questions, and implementation proposals. Treat it as the session's detailed source of truth.

## Concurrent sessions

Estimator/targeting work and the map inspector are developed by separate concurrent sessions. [`docs/coordination/viewer-estimator-contract.md`](docs/coordination/viewer-estimator-contract.md) is their shared protocol (file ownership, claims on shared files, additive diagnostics, the viewer/estimator data contract and open requests). Read it before touching diagnostics, the viewer, or scenario code, and stage explicit paths only when committing.

## Keeping this file current

Update this file when project goals, settled architecture, development workflow, or durable conventions change. Keep transient discussion and unresolved design exploration in the linked session state; promote settled, lasting decisions here when useful.
