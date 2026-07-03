use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

use yoke::cli;
use yoke::cli::{Command, WebhooksSubcommand};
use yoke::config::{Config, resolve_agents, validate_env_vars};
use yoke::harness::check_agent_health;
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
/// Signal handler installation happens outside `tokio::spawn` so that
/// installation errors are returned to the caller rather than swallowed
/// by the spawned task.
///
/// Returns a `JoinHandle` for the spawned signal handler task, or an error
/// if signal handler installation fails.
pub fn setup_signal_handler(
    shutdown_tx: watch::Sender<bool>,
) -> Result<tokio::task::JoinHandle<()>, Box<dyn std::error::Error + Send + Sync>> {
    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| format!("failed to install SIGINT handler: {e}"))?;
    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| format!("failed to install SIGTERM handler: {e}"))?;

    Ok(tokio::spawn(async move {
        // Wait for the first signal
        tokio::select! {
            _ = sigint.recv() => {
                tracing::info!("Shutdown signal received (SIGINT), starting graceful shutdown");
            }
            _ = sigterm.recv() => {
                tracing::info!("Shutdown signal received (SIGTERM), starting graceful shutdown");
            }
        }

        // First signal: trigger graceful shutdown
        let _ = shutdown_tx.send(true);

        // Wait for a second signal to force immediate exit
        tokio::select! {
            _ = sigint.recv() => {
                tracing::warn!("Second shutdown signal (SIGINT) received, forcing immediate exit");
                std::process::exit(1);
            }
            _ = sigterm.recv() => {
                tracing::warn!("Second shutdown signal (SIGTERM) received, forcing immediate exit");
                std::process::exit(1);
            }
            // Safety net: if no second signal arrives within 60s, exit normally
            _ = tokio::time::sleep(Duration::from_secs(60)) => {
                tracing::info!("Shutdown timeout reached in signal handler");
            }
        }
    }))
}

/// Wrap an error with additional context, preserving the full error chain.
///
/// The `{e:#}` format in `main()` will print:
/// `context: <original error Display>`
/// followed by each `source()` in turn, joined by `: `.
fn context<T, E>(context: &str, result: Result<T, E>) -> Result<T, ContextError>
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    result.map_err(|e| ContextError {
        context: context.to_string(),
        source: e.into(),
    })
}

/// A wrapper that prepends a context string to any error,
/// preserving the original as its `source()` so that `{e:#}` prints
/// the full chain: `context: original error: source: ...`
#[derive(Debug)]
struct ContextError {
    context: String,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.context, self.source)
    }
}

impl std::error::Error for ContextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Handle the `webhooks` subcommand.
///
/// Returns `Ok(())` if all per-repo operations succeeded, or
/// `Err(EXIT_WEBHOOK_ERRORS)` if any errors were recorded in the summary
/// (e.g. API auth failure, network error). This allows the caller to
/// exit with a non-zero status code so scripts and CI can detect failures.
async fn handle_webhooks_command(
    config: &Config,
    cmd: &WebhooksSubcommand,
    workflows_dir: &std::path::Path,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

    let client = webhooks::WebhookClient::new(&config.platform, &owner, gitlab_url)?;

    let had_errors = match cmd {
        WebhooksSubcommand::Add { workflows } => {
            let workflows_path = workflows.as_deref().unwrap_or(workflows_dir);
            let summary = webhooks::webhooks_add(config, &client, workflows_path).await?;
            summary.errors > 0
        }
        WebhooksSubcommand::Remove => {
            let summary = webhooks::webhooks_remove(config, &client).await?;
            summary.errors > 0
        }
        WebhooksSubcommand::List => {
            let summary = webhooks::webhooks_list(config, &client).await?;
            summary.errors > 0
        }
    };

    if had_errors {
        return Err(Box::new(std::io::Error::other(
            "webhook command completed with one or more errors",
        )));
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber for structured logging.
    // Timestamps in HH:MM:SS format (local time). RUST_LOG controls levels at runtime.
    // ANSI color codes are disabled when stderr is not a TTY (e.g. when run under
    // Ansible, systemd, or piped to a file) to keep log output clean.
    let timer = tracing_subscriber::fmt::time::LocalTime::new(
        time::format_description::parse("[hour]:[minute]:[second]").unwrap_or_else(|e| {
            eprintln!("Failed to parse time format: {e}. This is a bug; please report it.");
            std::process::exit(1);
        }),
    );
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_timer(timer)
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    if let Err(e) = run().await {
        // {e:#} on a boxed Error prints the full Display chain, surfacing
        // every layer of context so startup failures are easy to diagnose.
        eprintln!("Yoke failed to start: {e:#}");
        std::process::exit(1);
    }
}

/// Main application logic.
///
/// All startup and runtime errors are returned as `Result` rather than
/// logged-and-exited inline, so that `main()` can print them to stderr in
/// a clean, ANSI-free format suitable for non-TTY environments.
async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = cli::Cli::parse();

    // If a subcommand was provided, handle it and exit
    if let Some(Command::Webhooks(webhooks_cmd)) = &args.command {
        let config = context("failed to load config", Config::load(&args.config))?;

        context(
            "webhooks command failed",
            handle_webhooks_command(&config, &webhooks_cmd.command, &args.workflows).await,
        )?;
        return Ok(());
    }

    // Default behavior: start the server
    let mut config = context("failed to load config", Config::load(&args.config))?;

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

    // Validate required environment variables before starting
    context(
        "environment variable validation failed",
        validate_env_vars(&config.platform),
    )?;

    let workflows = context(
        "failed to load workflows",
        workflow::load_workflows(&args.workflows),
    )?;

    // Validate that all agents referenced in workflow steps exist in config
    let workflow_refs: Vec<workflow::Workflow> = workflows.iter().map(|(_, w)| w.clone()).collect();
    context(
        "agent resolution failed",
        resolve_agents(&config, &workflow_refs),
    )?;

    // Validate that all trigger types match the configured platform
    if let Err(msg) = workflow::validate_triggers(&config.platform, &workflows) {
        return Err(ContextError {
            context: "trigger validation failed".to_string(),
            source: Box::new(std::io::Error::other(msg)),
        }
        .into());
    }

    // Perform agent health checks — verify each configured agent is reachable
    // and reports a healthy status before starting the server.
    for agent in &config.agents {
        match check_agent_health(agent).await {
            Ok(health) => {
                tracing::info!(
                    agent = %agent.name,
                    platform = %health.platform,
                    version = %health.version,
                    "Agent health check passed"
                );
            }
            Err(e) => {
                return Err(ContextError {
                    context: "agent health check failed".to_string(),
                    source: Box::new(e),
                }
                .into());
            }
        }
    }

    tracing::info!(
        workflow_count = workflows.len(),
        "Configuration and workflow(s) loaded and validated successfully"
    );

    // Create shutdown watch channel — shared across signal handler, server, and dispatcher
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // Set up SIGINT/SIGTERM signal handler
    let _signal_handler = setup_signal_handler(shutdown_tx)?;
    // Create the global workflow state with ArcSwap for lock-free atomic updates
    let state = Arc::new(WorkflowState::new(workflows));

    // Set up file watcher for hot-reload of workflow TOML files
    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::channel(32);
    let _file_watcher = match reload::setup_file_watcher(&args.workflows, reload_tx) {
        Ok(w) => {
            tracing::info!(
                path = %args.workflows.display(),
                "Watching workflow directory for changes"
            );
            Some(w)
        }
        Err(e) => {
            tracing::warn!(
                path = %args.workflows.display(),
                error = %e,
                "Failed to set up file watcher; hot-reload disabled"
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
    server::run_server(&config, drain_timeout, shutdown_rx, state).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    #[tokio::test]
    async fn test_setup_signal_handler_returns_ok() {
        let (tx, _rx) = watch::channel(false);
        let handle = setup_signal_handler(tx);
        assert!(handle.is_ok());
        // Abort the spawned task to clean up
        if let Ok(h) = handle {
            h.abort();
        }
    }
}
