# Project instructions

## Project context

This repository is **Einsteinbot**, a Rust project to build an information-gathering bot for [NewtonWars](https://github.com/Draradech/NewtonWars). The NewtonWars source checkout is expected at `../NewtonWars` and is the reference for its build process, runtime behavior, and client protocol.

The project target is to infer planet locations and gravity effects efficiently, maintain an incrementally updated map with uncertainty, and use that model to improve shot selection. Reinforcement learning is explicitly a later possibility, not an initial requirement.

## Development direction

- First understand how to build and run the local NewtonWars game.
- Build a basic Rust telnet/TCP client and map the communication protocol before concentrating on bot strategy.
- Keep transport, protocol parsing, game state/map estimation, simulation, targeting, and bot policy modular, with clear interfaces.
- Keep early automated coverage small and focused on core behavior.

## Settled runtime contracts

- One CLI invocation owns one TCP connection. Multi-bot management is deferred; local demo dummies are started separately.
- The client uses the NewtonWars protocol version 8. Keep transport, typed protocol events, and bot policy in separate modules.
- SQLite recording defaults on and stores parsed semantic inbound events and outbound commands, ordered per connection. It does not store raw TCP bytes. `--no-record` disables it; `--dummy` opts into the simple energy-gated firing policy.
- The client rate limiter is per TCP connection: 16 outbound wire bytes per second with a 512-byte burst. Upstream refills one byte every three loop steps and exempts localhost, so its actual wall-clock rate depends on loop pace. Keep the client cap unless upstream source or limits change.
- NewtonWars only sends planet positions when launched with `sendplanets 1`.
- `Notes.md` is user-owned; do not edit it. Durable project operation belongs in README.md and lasting development conventions belong here.

## Verification

- Run `cargo fmt --check` and `cargo test` for Rust changes.
- Live client checks may use an already-running local NewtonWars listener on port 3490. Do not start a second server if one is already running.

## Session state

See [`docs/brainstorms/2026-11-13-newtonwars-bot.md`](docs/brainstorms/2026-11-13-newtonwars-bot.md) for the active design/interview state, evidence, decisions, open questions, and implementation proposals. Treat it as the session's detailed source of truth.

## Keeping this file current

Update this file when project goals, settled architecture, development workflow, or durable conventions change. Keep transient discussion and unresolved design exploration in the linked session state; promote settled, lasting decisions here when useful.
