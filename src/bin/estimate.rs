use einsteinbot::estimator::{fit_greedy_source_counts, shot_observations};
use einsteinbot::event_store::EventLog;
use einsteinbot::simulation::WorldParameters;
use std::path::PathBuf;

#[derive(Debug)]
struct Options {
    db_path: PathBuf,
    capture_id: i64,
    max_sources: usize,
    radius_prior: f64,
    grid_spacing: f64,
    validation_every: usize,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(error) => {
            eprintln!("estimate failed: {error}");
            std::process::exit(2);
        }
    }
}

fn run() -> Result<(), String> {
    let Some(options) = parse_args(std::env::args().skip(1))? else {
        return Ok(());
    };
    let events = EventLog::read_capture(&options.db_path, options.capture_id)
        .map_err(|error| format!("cannot read capture {}: {error}", options.capture_id))?;
    let shots = shot_observations(&events);
    if shots.is_empty() {
        return Err(format!(
            "capture {} has no completed protocol-version-8 shot paths",
            options.capture_id
        ));
    }

    let (training, validation) = split_shots(&shots, options.validation_every);
    let candidates = fit_greedy_source_counts(
        &training,
        &validation,
        WorldParameters::default(),
        options.max_sources,
        options.radius_prior,
        options.grid_spacing,
    )
    .map_err(|error| error.to_string())?;

    println!(
        "capture={} events={} training_shots={} validation_shots={}",
        options.capture_id,
        events.len(),
        training.len(),
        validation.len()
    );
    for candidate in candidates.candidates {
        let result = candidate.result;
        let param_count = result.map.planets.len() * 3;
        println!(
            "sources={} train_position_rmse={:.6} validation_position_rmse={} jacobian_rank={}/{} covariance={} converged={}",
            result.map.planets.len(),
            result.residual_rmse,
            candidate
                .validation_rmse
                .map(|value| format!("{value:.6}"))
                .unwrap_or_else(|| "unavailable".to_owned()),
            result.rank,
            param_count,
            if result.covariance.is_some() {
                "available"
            } else {
                "rank-deficient"
            },
            result.converged,
        );
        for (index, planet) in result.map.planets.iter().enumerate() {
            println!(
                "  planet[{index}] center=({:.3},{:.3}) radius_prior={:.3} mass={:.3}",
                planet.x, planet.y, planet.radius, planet.mass
            );
        }
    }
    println!("search_stopped={:?}", candidates.stop_reason);
    println!("World settings use NewtonWars source defaults; no oracle map is used for fitting.");
    Ok(())
}

fn parse_args<I>(args: I) -> Result<Option<Options>, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut db_path = PathBuf::from("einsteinbot.sqlite3");
    let mut capture_id = None;
    // Finite-difference fitting cost grows steeply with candidate count. Keep
    // the default practical for an interactive first pass; users can raise it.
    let mut max_sources = 4;
    let mut radius_prior = 30.0_f64;
    let mut grid_spacing = 80.0_f64;
    let mut validation_every = 5;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                return Ok(None);
            }
            "--db-path" => {
                db_path = PathBuf::from(value(&mut args, &arg)?);
            }
            "--capture-id" => {
                capture_id = Some(parse_value(&value(&mut args, &arg)?, &arg)?);
            }
            "--max-sources" => {
                max_sources = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--radius-prior" => {
                radius_prior = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--grid-spacing" => {
                grid_spacing = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            "--validation-every" => {
                validation_every = parse_value(&value(&mut args, &arg)?, &arg)?;
            }
            _ => return Err(format!("unknown option {arg}\n\n{}", help_text())),
        }
    }
    let capture_id = capture_id.ok_or_else(|| "--capture-id is required".to_owned())?;
    if max_sources == 0 || max_sources > 64 {
        return Err("--max-sources must be between 1 and 64".to_owned());
    }
    if !radius_prior.is_finite() || radius_prior <= 0.0 {
        return Err("--radius-prior must be finite and positive".to_owned());
    }
    if !grid_spacing.is_finite() || grid_spacing <= 0.0 {
        return Err("--grid-spacing must be finite and positive".to_owned());
    }
    if validation_every == 1 {
        return Err("--validation-every must be 0 or at least 2".to_owned());
    }
    Ok(Some(Options {
        db_path,
        capture_id,
        max_sources,
        radius_prior,
        grid_spacing,
        validation_every,
    }))
}

fn split_shots(
    shots: &[einsteinbot::simulation::ShotObservation],
    validation_every: usize,
) -> (
    Vec<einsteinbot::simulation::ShotObservation>,
    Vec<einsteinbot::simulation::ShotObservation>,
) {
    if validation_every == 0 || shots.len() < validation_every {
        return (shots.to_vec(), Vec::new());
    }
    let mut training = Vec::new();
    let mut validation = Vec::new();
    for (index, shot) in shots.iter().enumerate() {
        if (index + 1) % validation_every == 0 {
            validation.push(shot.clone());
        } else {
            training.push(shot.clone());
        }
    }
    if training.is_empty() || validation.is_empty() {
        (shots.to_vec(), Vec::new())
    } else {
        (training, validation)
    }
}

fn value<I>(args: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("missing value for {option}"))
}

fn parse_value<T>(value: &str, option: &str) -> Result<T, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| format!("invalid {option} value {value:?}: {error}"))
}

fn print_help() {
    print!("{}", help_text());
}

fn help_text() -> &'static str {
    "Usage: estimate --capture-id ID [OPTIONS]\n\
     \n\
     Options:\n\
       --db-path PATH          SQLite file (default: einsteinbot.sqlite3)\n\
       --capture-id ID         Capture to analyze (required)\n\
       --max-sources N         Maximum candidate planet count (default: 4)\n\
       --radius-prior VALUE    Radius used for seeding (default: 30)\n\
       --grid-spacing VALUE    Deterministic seed-grid spacing (default: 80)\n\
       --validation-every N    Hold out every Nth shot; 0 disables (default: 5)\n\
       -h, --help              Show this help\n"
}
