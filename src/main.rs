use einsteinbot::{client, config};

fn main() {
    match config::load() {
        Ok(Some(config)) => {
            if let Err(error) = client::run(&config) {
                eprintln!("Einsteinbot failed: {error}");
                std::process::exit(1);
            }
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("Configuration error: {error}");
            std::process::exit(2);
        }
    }
}
