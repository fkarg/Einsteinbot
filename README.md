# Einsteinbot

Rust client and eventual information-gathering bot for [NewtonWars](https://github.com/Draradech/NewtonWars).

## Diagnostic client

One invocation opens one TCP connection. It reads the server's text welcome prompt, enters bot protocol 8, prints parsed game events, and requests energy once per second. By default it is passive; `--dummy` enables small randomized shots when reported energy is at least 2.

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

The database stores one capture per connection. Each parsed server event and semantic client command gets a sequence number and timestamp. Raw TCP bytes are not stored. For a different database file, use `--db-path PATH`.

Custom names must be printable ASCII and at most 15 bytes, matching the NewtonWars server limit.

Options can come from a TOML file, environment variables, and command-line arguments. Later sources override earlier ones in this order: config file, environment, command line. For example:

```toml
# einsteinbot.toml
host = "127.0.0.1"
port = 3490
name = "Einstein"
record = true
db_path = "einsteinbot.sqlite3"
dummy = false
```

```sh
cargo run -- --config einsteinbot.toml
```

Set individual values with `EINSTEINBOT_HOST`, `EINSTEINBOT_PORT`, `EINSTEINBOT_NAME`, `EINSTEINBOT_RECORD`, `EINSTEINBOT_DUMMY`, and `EINSTEINBOT_DB_PATH`. Set `EINSTEINBOT_CONFIG` to choose a config file through the environment. Recording defaults to on; set `record = false` or `EINSTEINBOT_RECORD=false`, or pass `--no-record` to disable it. Dummy actions default to off; set `dummy = true`, `EINSTEINBOT_DUMMY=true`, or pass `--dummy` to enable them.

The dummy chooses an integer velocity between 2 and `min(8, floor(energy))` and an integer angle from -180 through 180, at most one shot per energy response. Every outbound command, including connection setup and energy requests, passes through a client-side token bucket capped at 16 wire bytes per second with at most 512 bytes of burst. The upstream server starts each connection with a 512-byte allowance and replenishes one byte every three game-loop iterations. That is nominally 20 bytes per second at 60 loops per second; its actual limit changes with loop pace. NewtonWars exempts local connections from enforcement, but this client applies its stricter cap locally too.

## Mapping library prototype

The `map` module computes NewtonWars planet acceleration and its first and second spatial derivatives. The `simulation` module reproduces the source's 25-substep velocity-Verlet path and exposes position and reconstructed-acceleration residuals. `estimator::fit_fixed_count` fits planet centers and gravity weights from completed shot paths and returns local parameter covariance when the fit is full-rank. The caller must provide an initial candidate map; this first fitter keeps source count and radii fixed. It is a library prototype and is not yet wired to a capture-analysis CLI or bot policy.

## Local NewtonWars server

The client does not start a server. From the NewtonWars checkout, the macOS source build is:

```sh
cd ../NewtonWars
./build-mac.sh
./nw fullscreen 0 sendplanets 1 throttle 16
```

The `sendplanets 1` option is required if the server should disclose planet positions. If a local server is already running, connect to it without starting another copy. Local NewtonWars connections may be run side by side; use separate Einsteinbot invocations for now.
