//! Development tool: offline evaluation and diagnosis subcommands. Truth (disclosed or
//! generated planets) only scores; it never reaches the estimator or the policy's belief.
mod config;
mod degrade;
mod eval;
mod games;
mod report;
mod schedule;
mod targeting;
mod world;

const USAGE: &str = "Usage: devtool <subcommand> [options] (--help after a subcommand for its options)
  eval            one config on one world at one fidelity: estimator and targeting metrics (JSON), cached
  degrade         D1-D6 degradation ladders: the truth map degraded, chosen shots flown on the truth
  games           independent games of a capture database (duplicate maps across captures merged)
  capture-report  recorded paths of a capture: repeats, coverage overlap, oracle path RMSE
  schedule        the estimator under live timing: one path every MS of wall clock, publish lag";

fn main() {
    let mut args = std::env::args().skip(1);
    let result = match args.next().as_deref() {
        Some("eval") => eval::parse(args).and_then(|a| a.map_or(Ok(()), eval::run)),
        Some("degrade") => degrade::run(args),
        Some("games") => games::run(args),
        Some("capture-report") => report::run(args).map_err(|e| e.to_string()),
        Some("schedule") => schedule::run(args),
        Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.to_owned()),
    };
    if let Err(error) = result {
        eprintln!("devtool: {error}");
        std::process::exit(2);
    }
}
