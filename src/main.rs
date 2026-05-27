use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;
use yoke::cli;
use yoke::config;
use yoke::config::Config;
use yoke::reload;
use yoke::reload::WorkflowState;
use yoke::server;
use yoke::workflow;

/// Set up the SIGINT/SIGTERM signal handler for graceful shutdown.
///
/// On the first SIGINT or SIGTERM, sends `true` on the watch channel to
/// trigger graceful shutdown across all components (HTTP server, dispatcher).
/// On a second signal, forces an immediate `process::exit(1)`.
///
/// Returns a `JoinHandle` for the spawned signal handler task.
pub fn setup_signal_handler(shutdown_tx: watch::Sender<bool>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut sigint = signal(SignalKind::interrupt()).expect("failed to install SIGINT handler");
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        // Wait for the first signal
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("SIGINT received, starting graceful shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM received, starting graceful shutdown");
            }
        }

        // First signal: trigger graceful shutdown
        let _ = shutdown_tx.send(true);

        // Wait for a second signal to force immediate exit
        tokio::select! {
            _ = sigint.recv() => {
                tracing::warn!("Second SIGINT received: forcing immediate exit");
                std::process::exit(1);
            }
            _ = sigterm.recv() => {
                tracing::warn!("Second SIGTERM received: forcing immediate exit");
                std::process::exit(1);
            }
            // Safety net: if no second signal arrives within 60s, exit normally
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                tracing::info!("Shutdown timeout reached in signal handler");
            }
        }
    })
}

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

    // Create shutdown watch channel — shared across signal handler, server, and dispatcher
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Set up SIGINT/SIGTERM signal handler
    let _signal_handler = setup_signal_handler(shutdown_tx);
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

    // Start the HTTP server with graceful shutdown
    let drain_timeout = Duration::from_secs(config.runtime.drain_timeout_secs);
    tracing::info!(
        "Starting server on {}:{} (drain timeout: {:?})",
        config.server.host,
        config.server.port,
        drain_timeout
    );
    if let Err(e) = server::run_server(
        &config.server,
        &config.platform,
        config.runtime.max_concurrent,
        PathBuf::from(&config.runtime.workdir),
        drain_timeout,
        shutdown_rx,
    )
    .await
    {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}
