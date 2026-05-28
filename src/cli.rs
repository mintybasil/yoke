use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Yoke agent orchestrator
#[derive(Parser, Debug)]
#[command(name = "yoke", about = "Yoke agent orchestrator", version)]
pub struct Cli {
    /// Path to config.toml
    #[arg(long, default_value = "config.toml")]
    pub config: PathBuf,

    /// Directory containing workflow TOML files
    #[arg(long, default_value = "./workflows")]
    pub workflows: PathBuf,

    /// Server bind address (overrides config.toml)
    #[arg(long)]
    pub host: Option<String>,

    /// External hostname for webhook URLs (overrides config.toml webhook_host)
    #[arg(long)]
    pub webhook_host: Option<String>,

    /// Server listen port (overrides config.toml)
    #[arg(long)]
    pub port: Option<u16>,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Root-level subcommands.
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Manage repository webhooks
    Webhooks(WebhooksCommand),
}

/// Manage repository webhooks on GitHub or GitLab.
#[derive(Parser, Debug)]
pub struct WebhooksCommand {
    #[command(subcommand)]
    pub command: WebhooksSubcommand,
}

/// Webhook management subcommands.
#[derive(Subcommand, Debug)]
pub enum WebhooksSubcommand {
    /// Add or update a webhook for the configured repositories (idempotent)
    Add {
        /// Path to workflow TOML directory to inspect for event types
        /// (defaults to the --workflows CLI arg)
        #[arg(long)]
        workflows: Option<PathBuf>,
    },
    /// Remove all webhooks matching Yoke's URL from configured repositories
    Remove,
    /// List existing webhooks for the configured repositories
    List,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_path() {
        let cli = Cli::parse_from::<_, &str>([]);
        assert_eq!(cli.config, PathBuf::from("config.toml"));
    }

    #[test]
    fn test_default_workflows_dir() {
        let cli = Cli::parse_from::<_, &str>([]);
        assert_eq!(cli.workflows, PathBuf::from("./workflows"));
    }

    #[test]
    fn test_custom_config_path() {
        let cli = Cli::parse_from(["yoke", "--config", "/path/to/config.toml"]);
        assert_eq!(cli.config, PathBuf::from("/path/to/config.toml"));
    }

    #[test]
    fn test_custom_workflows_dir() {
        let cli = Cli::parse_from(["yoke", "--workflows", "/path/to/workflows"]);
        assert_eq!(cli.workflows, PathBuf::from("/path/to/workflows"));
    }

    #[test]
    fn test_host_override() {
        let cli = Cli::parse_from(["yoke", "--host", "127.0.0.1"]);
        assert_eq!(cli.host, Some("127.0.0.1".to_string()));
    }

    #[test]
    fn test_port_override() {
        let cli = Cli::parse_from(["yoke", "--port", "9000"]);
        assert_eq!(cli.port, Some(9000));
    }

    #[test]
    fn test_host_and_port_overrides() {
        let cli = Cli::parse_from(["yoke", "--host", "0.0.0.0", "--port", "8080"]);
        assert_eq!(cli.host, Some("0.0.0.0".to_string()));
        assert_eq!(cli.port, Some(8080));
    }

    #[test]
    fn test_no_overrides() {
        let cli = Cli::parse_from::<_, &str>([]);
        assert!(cli.host.is_none());
        assert!(cli.webhook_host.is_none());
        assert!(cli.port.is_none());
    }

    #[test]
    fn test_webhook_host_override() {
        let cli = Cli::parse_from(["yoke", "--webhook-host", "yoke.example.com"]);
        assert_eq!(cli.webhook_host, Some("yoke.example.com".to_string()));
    }

    #[test]
    fn test_webhooks_add_subcommand() {
        let cli = Cli::parse_from(["yoke", "webhooks", "add"]);
        match cli.command {
            Some(Command::Webhooks(cmd)) => match cmd.command {
                WebhooksSubcommand::Add { workflows } => {
                    assert!(workflows.is_none());
                }
                _ => panic!("expected Add subcommand"),
            },
            _ => panic!("expected Webhooks command"),
        }
    }

    #[test]
    fn test_webhooks_add_with_workflows() {
        let cli = Cli::parse_from(["yoke", "webhooks", "add", "--workflows", "/path/to/wf"]);
        match cli.command {
            Some(Command::Webhooks(cmd)) => match cmd.command {
                WebhooksSubcommand::Add { workflows } => {
                    assert_eq!(workflows, Some(PathBuf::from("/path/to/wf")));
                }
                _ => panic!("expected Add subcommand"),
            },
            _ => panic!("expected Webhooks command"),
        }
    }

    #[test]
    fn test_webhooks_remove_subcommand() {
        let cli = Cli::parse_from(["yoke", "webhooks", "remove"]);
        match cli.command {
            Some(Command::Webhooks(cmd)) => match cmd.command {
                WebhooksSubcommand::Remove => {}
                _ => panic!("expected Remove subcommand"),
            },
            _ => panic!("expected Webhooks command"),
        }
    }

    #[test]
    fn test_webhooks_list_subcommand() {
        let cli = Cli::parse_from(["yoke", "webhooks", "list"]);
        match cli.command {
            Some(Command::Webhooks(cmd)) => match cmd.command {
                WebhooksSubcommand::List => {}
                _ => panic!("expected List subcommand"),
            },
            _ => panic!("expected Webhooks command"),
        }
    }

    #[test]
    fn test_no_subcommand() {
        let cli = Cli::parse_from::<_, &str>([]);
        assert!(cli.command.is_none());
    }
}
