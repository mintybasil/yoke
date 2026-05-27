use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use yoke::cli;
use yoke::config;
use yoke::config::Config;
use yoke::reload;
use yoke::reload::WorkflowState;
use yoke::server;
use yoke::workflow;

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

    // Create the global workflow state with ArcSwap for lock-free atomic updates
    let state = Arc::new(WorkflowState::new(workflows));

    // Set up file watcher for hot-reload of workflow TOML files
    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::channel(32);
    let _file_watcher = match reload::setup_file_watcher(&args.workflows, reload_tx) {
        Ok(w) => {
            tracing::info!(
                "Watching workflow directory for changes: {}",
                args.workflows.display()
            );
            Some(w)
        }
        Err(e) => {
            tracing::warn!(
                "Failed to set up file watcher for {}: {e}; hot-reload disabled",
                args.workflows.display()
            );
            None
        }
    };

    // Spawn reload handler that re-loads workflows and swaps state atomically.
    // If validation fails, the error is logged and the previous state is preserved.
    let reload_state = state.clone();
    let reload_config = config.clone();
    let reload_workflows_dir = args.workflows.clone();
    tokio::spawn(async move {
        while let Some(msg) = reload_rx.recv().await {
            match &msg {
                reload::ReloadMessage::FileChanged { path } => {
                    tracing::info!(path = %path.display(), "Workflow file changed, attempting reload...");
                }
                reload::ReloadMessage::FileRemoved { path } => {
                    tracing::info!(path = %path.display(), "Workflow file removed, attempting reload...");
                }
            }

            match reload::reload_workflows(&reload_workflows_dir, &reload_config) {
                Ok(new_workflows) => {
                    reload_state.update(new_workflows);
                    tracing::info!("Workflows reloaded successfully");
                }
                Err(e) => {
                    tracing::error!(error = %e, "Failed to reload workflows; keeping previous state");
                }
            }
        }
    });

    // Start the HTTP server
    tracing::info!(
        "Starting server on {}:{}",
        config.server.host,
        config.server.port
    );
    if let Err(e) = server::run_server(
        &config.server,
        &config.platform,
        config.runtime.max_concurrent,
        PathBuf::from(&config.runtime.workdir),
    )
    .await
    {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}
