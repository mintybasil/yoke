mod cli;
mod config;
mod server;
mod template;
mod workflow;

use clap::Parser;
use config::Config;

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for structured logging
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

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

    // Allow WEBHOOK_SECRET env var to override config.toml value
    if let Ok(secret) = std::env::var("WEBHOOK_SECRET") {
        config.server.webhook_secret = secret;
    }

    // Validate required environment variables before starting
    if let Err(e) = config::validate_env_vars(&config.platform) {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
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
    let workflow_refs: Vec<workflow::Workflow> = workflows.iter().map(|(_, w)| w.clone()).collect();
    if let Err(e) = config::resolve_agents(&config, &workflow_refs) {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    // Validate that all trigger types match the configured platform
    if let Err(e) = workflow::validate_triggers(&config.platform, &workflows) {
        eprintln!("Configuration error: {e}");
        std::process::exit(1);
    }

    tracing::info!(
        "Configuration and {} workflow(s) loaded and validated successfully",
        workflows.len()
    );

    // Start the HTTP server
    tracing::info!(
        "Starting server on {}:{}",
        config.server.host,
        config.server.port
    );
    if let Err(e) = server::run_server(&config.server).await {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}
