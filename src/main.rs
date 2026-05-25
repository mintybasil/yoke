mod config;
mod template;
mod workflow;

use config::Config;

fn main() {
    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());

    match Config::load(&config_path) {
        Ok(_config) => {
            println!("Configuration loaded successfully from {config_path}");
        }
        Err(e) => {
            eprintln!("Error loading config from {config_path}: {e}");
            std::process::exit(1);
        }
    }
}
