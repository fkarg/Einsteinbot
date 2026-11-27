use rand::seq::IndexedRandom;
use serde::Deserialize;
use std::path::PathBuf;

const SCIENTIST_NAMES: &[&str] = &[
    "Einstein", "Curie", "Galilei", "Feynman", "Noether", "Faraday", "Maxwell", "Hawking",
    "Turing", "Herschel", "Hubble",
];

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    pub record: bool,
    pub dummy: bool,
    pub targeting: bool,
    pub oracle_map_for_policy: bool,
    pub clients: usize,
    pub db_path: PathBuf,
    /// The known total planet count of the server's maps (its `planets` setting, 1-64), used as
    /// fixed by the estimator; `None`: estimated with a broad prior.
    pub planets: Option<usize>,
    /// Learn the planet count over the session (P25, `--learn-planets`): start at 24, one up on
    /// the count-limit warning, one down on the too-high signal. Ignored when `planets` is given.
    pub learn_planets: bool,
    /// Show the live terminal view when stdout is a terminal (`--no-tui`: plain logging).
    pub tui: bool,
}

#[derive(Debug, Default)]
struct CliOptions {
    config_path: Option<PathBuf>,
    host: Option<String>,
    port: Option<String>,
    name: Option<String>,
    record: Option<bool>,
    dummy: Option<bool>,
    targeting: Option<bool>,
    oracle_map_for_policy: Option<bool>,
    clients: Option<String>,
    db_path: Option<PathBuf>,
    planets: Option<String>,
    learn_planets: Option<bool>,
    tui: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileOptions {
    host: Option<String>,
    port: Option<u16>,
    name: Option<String>,
    record: Option<bool>,
    dummy: Option<bool>,
    targeting: Option<bool>,
    oracle_map_for_policy: Option<bool>,
    clients: Option<usize>,
    db_path: Option<PathBuf>,
    planets: Option<usize>,
    learn_planets: Option<bool>,
    tui: Option<bool>,
}

#[derive(Debug, Default)]
struct EnvOptions {
    host: Option<String>,
    port: Option<String>,
    name: Option<String>,
    record: Option<String>,
    dummy: Option<String>,
    targeting: Option<String>,
    oracle_map_for_policy: Option<String>,
    clients: Option<String>,
    db_path: Option<PathBuf>,
    planets: Option<String>,
    learn_planets: Option<String>,
    tui: Option<String>,
}

enum ParsedCli {
    Help,
    Options(CliOptions),
}

pub fn load() -> Result<Option<AppConfig>, String> {
    let cli = match parse_args(std::env::args().skip(1))? {
        ParsedCli::Help => {
            print_help();
            return Ok(None);
        }
        ParsedCli::Options(cli) => cli,
    };
    let env = EnvOptions::from_process()?;
    let config_path = cli
        .config_path
        .clone()
        .or_else(|| std::env::var_os("EINSTEINBOT_CONFIG").map(PathBuf::from));
    let file = match config_path {
        Some(path) => {
            let contents = std::fs::read_to_string(&path)
                .map_err(|error| format!("cannot read config {}: {error}", path.display()))?;
            toml::from_str(&contents)
                .map_err(|error| format!("cannot parse config {}: {error}", path.display()))?
        }
        None => FileOptions::default(),
    };

    resolve(cli, env, file).map(Some)
}

impl EnvOptions {
    fn from_process() -> Result<Self, String> {
        let get = |key: &str| std::env::var(key).ok();
        Ok(Self {
            host: get("EINSTEINBOT_HOST"),
            port: get("EINSTEINBOT_PORT"),
            name: get("EINSTEINBOT_NAME"),
            record: get("EINSTEINBOT_RECORD"),
            dummy: get("EINSTEINBOT_DUMMY"),
            targeting: get("EINSTEINBOT_TARGETING"),
            oracle_map_for_policy: get("EINSTEINBOT_ORACLE_MAP"),
            clients: get("EINSTEINBOT_CLIENTS"),
            db_path: get("EINSTEINBOT_DB_PATH").map(PathBuf::from),
            planets: get("EINSTEINBOT_PLANETS"),
            learn_planets: get("EINSTEINBOT_LEARN_PLANETS"),
            tui: get("EINSTEINBOT_TUI"),
        })
    }
}

fn resolve(cli: CliOptions, env: EnvOptions, file: FileOptions) -> Result<AppConfig, String> {
    let host = cli
        .host
        .or(env.host)
        .or(file.host)
        .unwrap_or_else(|| "127.0.0.1".to_owned());
    if host.trim().is_empty() {
        return Err("host cannot be empty".to_owned());
    }

    let port = if let Some(value) = cli.port {
        parse_port(&value, "--port")?
    } else if let Some(value) = env.port {
        parse_port(&value, "EINSTEINBOT_PORT")?
    } else {
        file.port.unwrap_or(3490)
    };
    if port == 0 {
        return Err("port must be greater than zero".to_owned());
    }

    let name = cli
        .name
        .or(env.name)
        .or(file.name)
        .unwrap_or_else(random_scientist_name);
    if name.trim().is_empty() {
        return Err("name cannot be empty".to_owned());
    }
    if name.len() > 15 {
        return Err("name cannot exceed NewtonWars' 15-byte limit".to_owned());
    }
    if !name.bytes().all(|byte| (b' '..=b'~').contains(&byte)) {
        return Err("name must contain printable ASCII only".to_owned());
    }
    let clients = if let Some(value) = cli.clients {
        parse_count(&value, "--clients")?
    } else if let Some(value) = env.clients {
        parse_count(&value, "EINSTEINBOT_CLIENTS")?
    } else {
        file.clients.unwrap_or(1)
    };
    if clients == 0 || clients > 16 {
        return Err("clients must be between 1 and 16".to_owned());
    }
    if clients > 1 && !is_loopback_host(&host) {
        return Err("multiple clients are supported only for a loopback host".to_owned());
    }
    let planets = if let Some(value) = cli.planets {
        Some(parse_count(&value, "--planets")?)
    } else if let Some(value) = env.planets {
        Some(parse_count(&value, "EINSTEINBOT_PLANETS")?)
    } else {
        file.planets
    };
    if planets.is_some_and(|n| !(1..=64).contains(&n)) {
        return Err("planets must be between 1 and 64 (the server's range)".to_owned());
    }

    Ok(AppConfig {
        host,
        port,
        name,
        record: if let Some(record) = cli.record {
            record
        } else if let Some(value) = env.record {
            parse_bool(&value, "EINSTEINBOT_RECORD")?
        } else {
            file.record.unwrap_or(true)
        },
        dummy: if let Some(dummy) = cli.dummy {
            dummy
        } else if let Some(value) = env.dummy {
            parse_bool(&value, "EINSTEINBOT_DUMMY")?
        } else {
            file.dummy.unwrap_or(false)
        },
        targeting: if let Some(targeting) = cli.targeting {
            targeting
        } else if let Some(value) = env.targeting {
            parse_bool(&value, "EINSTEINBOT_TARGETING")?
        } else {
            file.targeting.unwrap_or(true)
        },
        oracle_map_for_policy: if let Some(oracle_map) = cli.oracle_map_for_policy {
            oracle_map
        } else if let Some(value) = env.oracle_map_for_policy {
            parse_bool(&value, "EINSTEINBOT_ORACLE_MAP")?
        } else {
            file.oracle_map_for_policy.unwrap_or(false)
        },
        clients,
        db_path: cli
            .db_path
            .or(env.db_path)
            .or(file.db_path)
            .unwrap_or_else(|| PathBuf::from("einsteinbot.sqlite3")),
        planets,
        learn_planets: if let Some(learn) = cli.learn_planets {
            learn
        } else if let Some(value) = env.learn_planets {
            parse_bool(&value, "EINSTEINBOT_LEARN_PLANETS")?
        } else {
            file.learn_planets.unwrap_or(false)
        },
        tui: if let Some(tui) = cli.tui {
            tui
        } else if let Some(value) = env.tui {
            parse_bool(&value, "EINSTEINBOT_TUI")?
        } else {
            file.tui.unwrap_or(true)
        },
    })
}

fn random_scientist_name() -> String {
    SCIENTIST_NAMES
        .choose(&mut rand::rng())
        .expect("scientist name list is not empty")
        .to_string()
}

fn parse_args<I>(args: I) -> Result<ParsedCli, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut options = CliOptions::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(ParsedCli::Help),
            "--config" => options.config_path = Some(PathBuf::from(value(&mut args, &arg)?)),
            "--host" => options.host = Some(value(&mut args, &arg)?),
            "--port" => options.port = Some(value(&mut args, &arg)?),
            "--name" => options.name = Some(value(&mut args, &arg)?),
            "--db-path" => options.db_path = Some(PathBuf::from(value(&mut args, &arg)?)),
            "--no-record" => options.record = Some(false),
            "--dummy" => options.dummy = Some(true),
            "--no-dummy" => options.dummy = Some(false),
            "--target" => options.targeting = Some(true),
            "--no-target" => options.targeting = Some(false),
            "--oracle-map" => options.oracle_map_for_policy = Some(true),
            "--no-oracle-map" => options.oracle_map_for_policy = Some(false),
            "--clients" => options.clients = Some(value(&mut args, &arg)?),
            "--planets" => options.planets = Some(value(&mut args, &arg)?),
            "--learn-planets" => options.learn_planets = Some(true),
            "--tui" => options.tui = Some(true),
            "--no-tui" => options.tui = Some(false),
            _ => return Err(format!("unknown argument: {arg}\n\n{}", help_text())),
        }
    }
    Ok(ParsedCli::Options(options))
}

fn parse_count(value: &str, source: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid {source} value {value:?}: {error}"))
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn value<I>(args: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| format!("missing value for {option}"))
}

fn parse_bool(value: &str, source: &str) -> Result<bool, String> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(true),
        "false" | "0" | "no" => Ok(false),
        _ => Err(format!(
            "invalid {source} value {value:?}; use true or false"
        )),
    }
}

fn parse_port(value: &str, source: &str) -> Result<u16, String> {
    value
        .parse::<u16>()
        .map_err(|error| format!("invalid {source} value {value:?}: {error}"))
}

fn print_help() {
    print!("{}", help_text());
}

fn help_text() -> &'static str {
    "Usage: einsteinbot [OPTIONS]\n\
     \n\
     Options:\n\
       --config PATH      Read TOML configuration\n\
       --host HOST        NewtonWars host (default: 127.0.0.1)\n\
       --port PORT        NewtonWars port (default: 3490)\n\
       --name NAME        Bot name (default: random scientist name)\n\
       --no-record         Disable SQLite recording\n\
       --dummy             Fire low-energy random shots\n\
       --no-dummy          Disable dummy actions\n\
       --target            Target players using trajectory search (default)\n\
       --no-target         Disable targeting (observe only)\n\
       --oracle-map        Allow local Planets events to drive targeting\n\
       --no-oracle-map     Ignore Planets events for targeting (default)\n\
       --clients N         Run N local bot connections (default: 1, max: 16)\n\
       --db-path PATH      SQLite file (default: einsteinbot.sqlite3)\n\
       --planets N         Known planet count of the server's maps (fixed; default: estimated)\n\
       --learn-planets     Learn the planet count over the session from 24 (ignored with --planets)\n\
       --no-tui            Plain logging instead of the live terminal view\n\
       -h, --help          Show this help\n\
     \n\
     Precedence: command line, environment, TOML config, defaults.\n\
     Environment: EINSTEINBOT_CONFIG, _HOST, _PORT, _NAME, _RECORD, _DUMMY, _TARGETING, _ORACLE_MAP, _CLIENTS, _DB_PATH, _PLANETS, _LEARN_PLANETS, _TUI.\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_line_values_override_environment_and_config() {
        let config = resolve(
            CliOptions {
                name: Some("Einstein".to_owned()),
                port: Some("3491".to_owned()),
                ..CliOptions::default()
            },
            EnvOptions {
                name: Some("Curie".to_owned()),
                port: Some("3492".to_owned()),
                ..EnvOptions::default()
            },
            FileOptions {
                name: Some("Turing".to_owned()),
                port: Some(3493),
                ..FileOptions::default()
            },
        )
        .unwrap();

        assert_eq!(config.name, "Einstein");
        assert_eq!(config.port, 3491);
        assert!(config.record);
    }

    #[test]
    fn random_scientist_list_excludes_newton() {
        assert!(
            !SCIENTIST_NAMES
                .iter()
                .any(|name| name.to_ascii_lowercase().contains("newton"))
        );
        assert!(SCIENTIST_NAMES.contains(&random_scientist_name().as_str()));
    }

    #[test]
    fn rejects_names_the_server_would_truncate() {
        let result = resolve(
            CliOptions {
                name: Some("sixteen characters".to_owned()),
                ..CliOptions::default()
            },
            EnvOptions::default(),
            FileOptions::default(),
        );

        assert!(result.unwrap_err().contains("15-byte limit"));
    }

    #[test]
    fn parses_env_values_and_cli_overrides_invalid_env_values() {
        let env = EnvOptions {
            port: Some("3499".to_owned()),
            record: Some("yes".to_owned()),
            ..EnvOptions::default()
        };
        let config = resolve(CliOptions::default(), env, FileOptions::default()).unwrap();

        assert_eq!(config.port, 3499);
        assert!(config.record);

        let env = EnvOptions {
            port: Some("not-a-port".to_owned()),
            record: Some("not-a-bool".to_owned()),
            ..EnvOptions::default()
        };
        let config = resolve(
            CliOptions {
                port: Some("3500".to_owned()),
                record: Some(false),
                ..CliOptions::default()
            },
            env,
            FileOptions::default(),
        )
        .unwrap();

        assert_eq!(config.port, 3500);
        assert!(!config.record);
    }

    #[test]
    fn no_record_flag_disables_default_recording() {
        let ParsedCli::Options(cli) = parse_args(["--no-record".to_owned()]).unwrap() else {
            panic!("expected parsed options");
        };
        let config = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();

        assert!(!config.record);
    }

    #[test]
    fn live_view_defaults_on_and_no_tui_turns_it_off() {
        let config = resolve(
            CliOptions::default(),
            EnvOptions::default(),
            FileOptions::default(),
        )
        .unwrap();
        assert!(config.tui);
        let ParsedCli::Options(cli) = parse_args(["--no-tui".to_owned()]).unwrap() else {
            panic!("expected options");
        };
        let config = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();
        assert!(!config.tui);
    }

    #[test]
    fn dummy_mode_is_opt_in_and_can_be_configured() {
        let passive = resolve(
            CliOptions::default(),
            EnvOptions::default(),
            FileOptions::default(),
        )
        .unwrap();
        assert!(!passive.dummy);

        let ParsedCli::Options(cli) = parse_args(["--dummy".to_owned()]).unwrap() else {
            panic!("expected parsed options");
        };
        let dummy = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();
        assert!(dummy.dummy);
    }

    #[test]
    fn targeting_is_on_by_default_and_oracle_map_requires_an_explicit_flag() {
        let passive = resolve(
            CliOptions::default(),
            EnvOptions::default(),
            FileOptions::default(),
        )
        .unwrap();
        assert!(passive.targeting);
        assert!(!passive.oracle_map_for_policy);

        let ParsedCli::Options(cli) = parse_args(["--no-target"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        let observer = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();
        assert!(!observer.targeting);

        let ParsedCli::Options(cli) =
            parse_args(["--target", "--oracle-map"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        let config = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();
        assert!(config.targeting);
        assert!(config.oracle_map_for_policy);
    }

    #[test]
    fn multiple_clients_require_loopback_and_have_a_small_cap() {
        let ParsedCli::Options(cli) = parse_args(["--clients", "2"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        let config = resolve(cli, EnvOptions::default(), FileOptions::default()).unwrap();
        assert_eq!(config.clients, 2);

        let ParsedCli::Options(cli) =
            parse_args(["--host", "example.com", "--clients", "2"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        assert!(resolve(cli, EnvOptions::default(), FileOptions::default()).is_err());

        let ParsedCli::Options(cli) = parse_args(["--clients", "17"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        assert!(resolve(cli, EnvOptions::default(), FileOptions::default()).is_err());
    }

    #[test]
    fn learning_the_planet_count_is_opt_in_from_any_source() {
        let resolve_with = |cli: &[&str], env: Option<&str>, file: &str| {
            let ParsedCli::Options(cli) = parse_args(cli.iter().map(|a| a.to_string())).unwrap()
            else {
                panic!("expected parsed options");
            };
            let env = EnvOptions {
                learn_planets: env.map(str::to_owned),
                ..EnvOptions::default()
            };
            resolve(cli, env, toml::from_str(file).unwrap())
                .unwrap()
                .learn_planets
        };
        assert!(!resolve_with(&[], None, ""));
        assert!(resolve_with(&["--learn-planets"], Some("false"), ""));
        assert!(resolve_with(&[], Some("1"), "learn_planets = false\n"));
        assert!(resolve_with(&[], None, "learn_planets = true\n"));
    }

    #[test]
    fn planet_count_is_optional_and_within_the_server_range() {
        let default = resolve(
            CliOptions::default(),
            EnvOptions::default(),
            FileOptions::default(),
        )
        .unwrap();
        assert_eq!(default.planets, None);
        let ParsedCli::Options(cli) = parse_args(["--planets", "26"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        let file: FileOptions = toml::from_str("planets = 22\n").unwrap();
        assert_eq!(
            resolve(cli, EnvOptions::default(), file).unwrap().planets,
            Some(26)
        );
        let file: FileOptions = toml::from_str("planets = 22\n").unwrap();
        assert_eq!(
            resolve(CliOptions::default(), EnvOptions::default(), file)
                .unwrap()
                .planets,
            Some(22)
        );
        let ParsedCli::Options(cli) = parse_args(["--planets", "65"].map(str::to_owned)).unwrap()
        else {
            panic!("expected parsed options");
        };
        assert!(resolve(cli, EnvOptions::default(), FileOptions::default()).is_err());
    }

    #[test]
    fn parses_toml_configuration() {
        let file: FileOptions = toml::from_str(
            "host = 'localhost'\nport = 3495\nname = 'Turing'\nrecord = false\ndummy = true\n",
        )
        .unwrap();
        let config = resolve(CliOptions::default(), EnvOptions::default(), file).unwrap();

        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 3495);
        assert_eq!(config.name, "Turing");
        assert!(!config.record);
        assert!(config.dummy);
    }
}
