mod cli;
mod config;
mod template;
mod workflow;

use clap::Parser;
use config::Config;

fn main() {
    let args = cli::Cli::parse();

    let mut config = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config from {}: {e}", args.config.display());
            std::process::exit(1);
        }
    };

    // Apply CLI overrides for server settings
    if let Some(host) = args.host {
        config.server.host = host;
    }
    if let Some(port) = args.port {
        config.server.port = port;
    }

    let workflows = match workflow::load_workflows(&args.workflows) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "Error loading workflows from {}: {e}",
                args.workflows.display()
            );
            std::process::exit(1);
        }
    };

    // Validate that all agents referenced in workflow steps exist in config
    if let Err(e) = config::resolve_agents(&config, &workflows) {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    println!(
        "Configuration and {} workflow(s) loaded and validated successfully",
        workflows.len()
    );
}
