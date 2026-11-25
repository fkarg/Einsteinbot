# Estimator baseline, 2026-11-25

This is a focused first measurement, not yet a complete replacement scorecard. The primary metric is gravity vector RMSE in explored cells; the unexplored and whole-field metrics are separate. Truth is used for scoring and map-epoch boundaries, not fitting. Results are specific to these captures and do not establish general performance.

## Inputs and configuration

- Source revision: `a5217c28b567ccf46c8a07d48df8968fee21742b`; Rust source was clean, with unrelated README and session-note edits and an untracked sweep tool. Release build on arm64; machine was not isolated and thread count was not recorded.
- `einsteinbot.sqlite3` SHA-256 `5c9c0bae8458e82862940e66e8244b6cdb22bae28c7bc6822daee0108f5547ed`; captures 44 and 49. Only the first eight accepted completed paths in each were scored. Capture 52 remains held out.
- `learning-scenarios.sqlite3` SHA-256 `b31afed8e553a158e6d59b320c717955adfb0bf5258db68d45dd99a6b724a557`; capture 1 is a known flat control, 12 is two-planet flybys, 18 is the 24-planet W7 scenario. These mutable databases are gitignored: hashes identify these versions but do not make them recoverable. Secure a durable copy before relying on this baseline across machines.
- Commands: `cargo run --release --bin field_scorecard -- --db-path DB --capture-id ID --seed grid`, with `--max-completed 8` on live captures and `--empty-is-flat` only for the known flat control. Grid is the live initializer. The scorer enables an inspection-only provisional source that live disables; its synchronous `fit_ms` includes that extra work and is not live latency. The table uses its no-source model (physical planets plus matching local residual), not the provisional model. The validated-source model was unavailable at these final checkpoints.

## Field measurements

Each row is the last completed-path checkpoint scored, not a whole-capture aggregate. Acceleration vector RMSE uses a common 20-unit grid, excluding truth planet interiors; larger errors are worse. `fit_ms` is cumulative estimator observation time, excluding scoring, and is a single run.

| Case | Sequence | Completed paths | Physical planets | Explored RMSE | Unexplored RMSE | Whole RMSE | Cumulative fit ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Synthetic 1, flat | 11 | 2 | 0 | 0 | 0 | 0 | 0.597 |
| Synthetic 12, two flybys | 15 | 3 | 1 | 0.033660 | 0.408236 | 0.397824 | 76.948 |
| Synthetic 18, W7 | 39 | 9 | 7 | 0.563619 | 3.363935 | 3.231218 | 60103.968 |
| Live 44, prefix | 124 | 8 | 6 | 0.807710 | 99.253990 | 98.153860 | 321.867 |
| Live 49, prefix | 105 | 8 | 5 | 0.779281 | 3.558598 | 3.453647 | 2105.352 |

W7 includes nine completed paths, including the subsequent trial shot; eight are the fixture's training paths. It printed three rejected physical fits before publishing later fits. Live 49 printed one rejected physical fit. These errors retain the contact-seeded partial map; they are not omitted successful fits. Live 44's large whole-field error warrants inspecting its spatial tail before drawing a broader conclusion.

The live 44 prefix was repeated using the same command. All 25 JSON rows were identical after removing cumulative `fit_ms`; cumulative time was 321.867 ms versus 320.156 ms. This one repeat does not establish timing stability or determinism on other cases.

## Separate diagnostics and gaps

For synthetic 12, `cargo run --release --bin targeting_report -- --db-path learning-scenarios.sqlite3 --capture-id 12 --fit-planets` reported one inferred planet matched to one of two truth planets at the final checkpoint, center error 14.167 and radius error 10.000, using maximum-cardinality matching within 50 units. It scored zero future trajectories because no usable prior physical map existed. Its prediction snapshot is before shot completion, not necessarily before launch, so overlapping shots can include information unavailable at launch. Fit RMSE is training error. These numbers are diagnostics, not causal shot-prediction quality.

For the same case, `cargo run --release --bin estimate -- --db-path learning-scenarios.sqlite3 --capture-id 12 --online --speed 0 --fit-patience 100` took 66 ms elapsed; the three shot-event service times had median 4.648 ms and maximum 61.251 ms. The final event spent 58.447 ms in planet fitting, including 55.490 ms optimizer time. Unpaced replay creates an artificial arrival pattern and does not measure live map age or targeting contention. The profiler explicitly sets patience 100; the installed `levenberg-marquardt` library's `new()` default is also 100, as used by live code. This equality is for the installed dependency, not a general guarantee.

The field scorer's pre-launch snapshots are causal, but the reported table uses post-completion checkpoints and does not measure held-out trajectory or collision outcomes. The current reports do not provide aligned live-paced published-belief field accuracy, map age, calibrated uncertainty, or collision-aware held-out trajectory scores. The live cases above are short prefixes; a longer growing-history measurement is still needed. Existing scorer coverage includes an oracle-independence test for a flyby; this session did not rerun that test or prove isolation for every path. No threshold or replacement decision is inferred from this first pass.
