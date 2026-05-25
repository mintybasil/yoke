use std::path::PathBuf;

use clap::Parser;

/// Yoke agent orchestrator
#[derive(Parser, Debug)]
#[command(name = "yoke", about = "Yoke agent orchestrator", version)]
pub struct Cli {
    /// Path to config.toml
    #[arg(long, default_value = "config.toml")]
    pub config: PathBuf,

    /// Directory containing workflow TOML files
    #[arg(long, default_value = ".")]
    pub workflows: PathBuf,

    /// Server bind address (overrides config.toml)
    #[arg(long)]
    pub host: Option<String>,

    /// Server listen port (overrides config.toml)
    #[arg(long)]
    pub port: Option<u16>,
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
        assert_eq!(cli.workflows, PathBuf::from("."));
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
        assert!(cli.port.is_none());
    }
}
