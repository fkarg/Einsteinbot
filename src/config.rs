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
    pub db_path: PathBuf,
}

#[derive(Debug, Default)]
struct CliOptions {
    config_path: Option<PathBuf>,
    host: Option<String>,
    port: Option<String>,
    name: Option<String>,
    record: Option<bool>,
    dummy: Option<bool>,
    db_path: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileOptions {
    host: Option<String>,
    port: Option<u16>,
    name: Option<String>,
    record: Option<bool>,
    dummy: Option<bool>,
    db_path: Option<PathBuf>,
}

#[derive(Debug, Default)]
struct EnvOptions {
    host: Option<String>,
    port: Option<String>,
    name: Option<String>,
    record: Option<String>,
    dummy: Option<String>,
    db_path: Option<PathBuf>,
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
            db_path: get("EINSTEINBOT_DB_PATH").map(PathBuf::from),
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
        db_path: cli
            .db_path
            .or(env.db_path)
            .or(file.db_path)
            .unwrap_or_else(|| PathBuf::from("einsteinbot.sqlite3")),
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
            _ => return Err(format!("unknown argument: {arg}\n\n{}", help_text())),
        }
    }
    Ok(ParsedCli::Options(options))
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
       --db-path PATH      SQLite file (default: einsteinbot.sqlite3)\n\
       -h, --help          Show this help\n\
     \n\
     Precedence: command line, environment, TOML config, defaults.\n\
     Environment: EINSTEINBOT_CONFIG, _HOST, _PORT, _NAME, _RECORD, _DUMMY, _DB_PATH.\n"
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
