use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use yoke::cli;
use yoke::cli::{Command, WebhooksSubcommand};
use yoke::config;
use yoke::config::Config;
use yoke::reload;
use yoke::reload::WorkflowState;
use yoke::server;
use yoke::webhooks;
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

/// Handle the `webhooks` subcommand.
async fn handle_webhooks_command(
    config: &Config,
    cmd: &WebhooksSubcommand,
    workflows_dir: &std::path::Path,
) {
    // Determine the gitlab_url for GitLab platform
    let gitlab_url = config.gitlab_url.as_ref().map(|u| {
        let s = u.to_string();
        format!("{}/api/v4", s.trim_end_matches('/'))
    });
    // For GitHub, extract owner from the first repo in config (or empty string if none)
    let owner = config
        .repos
        .first()
        .map(|r| r.owner.clone())
        .unwrap_or_default();

    let client = match webhooks::WebhookClient::new(&config.platform, &owner, gitlab_url) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "Error creating webhook client");
            std::process::exit(1);
        }
    };

    match cmd {
        WebhooksSubcommand::Add { workflows } => {
            let workflows_path = workflows.as_deref().unwrap_or(workflows_dir);
            if let Err(e) = webhooks::webhooks_add(config, &client, workflows_path).await {
                tracing::error!(error = %e, "Error adding webhooks");
                std::process::exit(1);
            }
        }
        WebhooksSubcommand::Remove => {
            if let Err(e) = webhooks::webhooks_remove(config, &client).await {
                tracing::error!(error = %e, "Error removing webhooks");
                std::process::exit(1);
            }
        }
        WebhooksSubcommand::List => {
            if let Err(e) = webhooks::webhooks_list(config, &client).await {
                tracing::error!(error = %e, "Error listing webhooks");
                std::process::exit(1);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for structured logging
    // Timestamps in HH:MM:SS format (local time). RUST_LOG controls levels at runtime.
    let timer = tracing_subscriber::fmt::time::LocalTime::new(
        time::format_description::parse("[hour]:[minute]:[second]").expect("valid time format"),
    );
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(timer)
        .init();

    let args = cli::Cli::parse();

    // If a subcommand was provided, handle it and exit
    if let Some(Command::Webhooks(webhooks_cmd)) = &args.command {
        let config = match Config::load(&args.config) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(path = %args.config.display(), error = %e, "Error loading config");
                std::process::exit(1);
            }
        };

        handle_webhooks_command(&config, &webhooks_cmd.command, &args.workflows).await;
        return;
    }

    // Default behavior: start the server
    let mut config = match Config::load(&args.config) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(path = %args.config.display(), error = %e, "Error loading config");
            std::process::exit(1);
        }
    };

    // Apply CLI overrides for server settings
    if let Some(host) = args.host {
        config.server.host = host;
    }
    if let Some(webhook_host) = args.webhook_host {
        config.server.webhook_host = webhook_host;
    }
    if let Some(port) = args.port {
        config.server.port = port;
    }

    // Allow WEBHOOK_SECRET env var to override config.toml value
    if let Ok(secret) = std::env::var(yoke::config::env::WEBHOOK_SECRET) {
        config.server.webhook_secret = secret;
    }

    // Validate that a webhook secret is available from either config or env var
    if config.server.webhook_secret.is_empty() {
        tracing::error!(
            "Webhook secret must be provided via config.toml or WEBHOOK_SECRET env var"
        );
        std::process::exit(1);
    }

    // Validate required environment variables before starting
    if let Err(e) = config::validate_env_vars(&config.platform) {
        tracing::error!(error = %e, "Configuration error");
        std::process::exit(1);
    }

    let workflows = match workflow::load_workflows(&args.workflows) {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(
                path = %args.workflows.display(),
                error = %e,
                "Error loading workflows"
            );
            std::process::exit(1);
        }
    };

    // Validate that all agents referenced in workflow steps exist in config
    let workflow_refs: Vec<workflow::Workflow> = workflows.iter().map(|(_, w)| w.clone()).collect();
    if let Err(e) = config::resolve_agents(&config, &workflow_refs) {
        tracing::error!(error = %e, "Configuration error");
        std::process::exit(1);
    }

    // Validate that all trigger types match the configured platform
    if let Err(e) = workflow::validate_triggers(&config.platform, &workflows) {
        tracing::error!(error = %e, "Configuration error");
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
        host = %config.server.host,
        port = %config.server.port,
        webhook_host = %config.server.webhook_host,
        drain_timeout = %config.runtime.drain_timeout_secs,
        "Starting server...",
    );
    if let Err(e) = server::run_server(
        &config.server,
        &config.platform,
        config.runtime.max_concurrent,
        PathBuf::from(&config.runtime.workdir),
        drain_timeout,
        shutdown_rx,
        state,
        config.agents.clone(),
    )
    .await
    {
        tracing::error!(error = %e, "Server error");
        std::process::exit(1);
    }
}
