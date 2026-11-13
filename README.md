# Einsteinbot

Rust client and eventual information-gathering bot for [NewtonWars](https://github.com/Draradech/NewtonWars).

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

Targeting is opt-in with `--target` or `targeting = true`. It searches legal integer velocities and 10-degree angles through the NewtonWars simulator, then picks a predicted hit against the latest known player positions. If it sees no opponent or finds no usable action, it falls back to the random dummy. An information bonus is considered only when no predicted hit is available and the average field uncertainty along the path clears a threshold. This first bonus uses marginal parameter variances as an uncertainty-exposure proxy, not a full posterior information-gain calculation.

The client does not yet have an online map-estimate producer. Without the explicit local-test option `--oracle-map`, the target policy currently simulates a flat field (no planets) and prints that limitation when it starts. `--oracle-map` allows a received `Planets` message to drive local target simulation; do not use it as a production assumption. The policy API accepts a map and parameter covariance, so a later online estimator can supply both. `--dummy` takes precedence if combined with `--target`.

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
