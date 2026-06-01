use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::workflow::Workflow;


/// Environment variable names used by Yoke.
pub mod env {
    /// Hermes API key (always required).
    pub const HERMES_API_KEY: &str = "HERMES_API_KEY";
    /// GitHub personal access token (required when platform = "github").
    pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";
    /// GitLab personal access token (required when platform = "gitlab").
    pub const GITLAB_TOKEN: &str = "GITLAB_TOKEN";
    /// Optional webhook secret override (overrides `server.webhook_secret` from config).
    pub const WEBHOOK_SECRET: &str = "WEBHOOK_SECRET";
}

/// Supported code platforms.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Platform {
    #[default]
    Github,
    Gitlab,
}

/// A repository to monitor.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Repo {
    pub owner: String,
    pub repo: String,
}

/// A named Hermes agent instance.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AgentConfig {
    pub name: String,
    pub base_url: Url,
}

/// Runtime settings. All fields have sensible defaults.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct RuntimeConfig {
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
    #[serde(default = "default_workdir")]
    pub workdir: String,
    /// Maximum time (in seconds) to wait for in-flight workflows to complete
    /// during graceful shutdown. Default: 30 seconds.
    #[serde(default = "default_drain_timeout_secs")]
    pub drain_timeout_secs: u64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_max_concurrent(),
            workdir: default_workdir(),
            drain_timeout_secs: default_drain_timeout_secs(),
        }
    }
}

fn default_max_concurrent() -> usize {
    0
}

fn default_drain_timeout_secs() -> u64 {
    30
}

fn default_workdir() -> String {
    "~/.yoke".to_string()
}

/// HTTP server settings.
///
/// - `host` is the bind address for the TCP listener (e.g. `"0.0.0.0"`).
/// - `webhook_host` is the external hostname used in webhook registration URLs
///   (e.g. `"yoke.example.com"`). This must be set explicitly — it determines
///   the public-facing hostname that platforms use to deliver webhook events.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub webhook_host: String,
    pub webhook_secret: String,
    #[serde(default = "default_max_body_size")]
    pub max_body_size: u64,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8644
}

fn default_max_body_size() -> u64 {
    1_048_576
}

/// GitHub-specific configuration. Currently a placeholder for future
/// configuration options (e.g. custom API endpoints, enterprise URLs).
#[derive(Debug, Deserialize, Serialize, Clone, Default)]
pub struct GithubConfig {}

/// GitLab-specific configuration.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GitlabConfig {
    #[serde(default = "default_gitlab_url")]
    pub gitlab_url: Url,
}

fn default_gitlab_url() -> Url {
    Url::parse("https://gitlab.com").expect("hardcoded default URL should parse")
}

/// Top-level configuration mirroring `config.toml`.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    pub platform: Platform,
    #[serde(default)]
    pub repos: Vec<Repo>,
    pub agents: Vec<AgentConfig>,
    #[serde(default)]
    pub runtime: RuntimeConfig,
    pub server: ServerConfig,
    /// GitHub-specific config (only used when platform = "github").
    #[serde(default)]
    pub github: Option<GithubConfig>,
    /// GitLab-specific config (only used when platform = "gitlab").
    #[serde(default)]
    pub gitlab: Option<GitlabConfig>,
    /// Top-level gitlab_url field for convenience (overrides gitlab.gitlab_url).
    #[serde(default)]
    pub gitlab_url: Option<Url>,
}

impl Config {
    /// Load configuration from a TOML file on disk.
    ///
    /// Performs tilde expansion on the `runtime.workdir` path.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = fs::read_to_string(path).map_err(ConfigError::Io)?;
        Self::from_str(&content)
    }

    /// Parse configuration from a TOML string (useful for tests).
    ///
    /// Performs tilde expansion on the `runtime.workdir` path.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(content: &str) -> Result<Self, ConfigError> {
        let mut config: Config = toml::from_str(content).map_err(ConfigError::Parse)?;
        config.validate()?;
        // Tilde expansion for workdir
        config.runtime.workdir = shellexpand::full(&config.runtime.workdir)
            .map_err(|e| ConfigError::ShellExpand(e.to_string()))?
            .to_string();
        Ok(config)
    }

    /// Validate the configuration beyond what serde enforces.
    fn validate(&self) -> Result<(), ConfigError> {
        // Agents must have unique names
        let mut seen = std::collections::HashSet::new();
        for agent in &self.agents {
            if !seen.insert(&agent.name) {
                return Err(ConfigError::Validation(format!(
                    "duplicate agent name: '{}'",
                    agent.name
                )));
            }
        }

        // At least one agent is required
        if self.agents.is_empty() {
            return Err(ConfigError::Validation(
                "at least one agent is required".to_string(),
            ));
        }

        // Verify agent base_urls have a scheme (http/https)
        for agent in &self.agents {
            let scheme = agent.base_url.scheme();
            if scheme != "http" && scheme != "https" {
                return Err(ConfigError::Validation(format!(
                    "agent '{}' has invalid URL scheme '{}', expected http or https",
                    agent.name, scheme
                )));
            }
        }

        Ok(())
    }
}

/// Errors that can occur during configuration loading or validation.
#[derive(Debug)]
pub enum ConfigError {
    /// I/O error reading the config file.
    Io(std::io::Error),
    /// TOML parse or deserialize error.
    Parse(toml::de::Error),
    /// Semantic validation error.
    Validation(String),
    /// Shell expansion error (e.g. unresolvable tilde).
    ShellExpand(String),
    /// Agent resolution error (workflow references unknown agent).
    AgentResolution(String),
    /// Missing required environment variable.
    EnvVar(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "I/O error: {e}"),
            ConfigError::Parse(e) => write!(f, "config parse error: {e}"),
            ConfigError::Validation(msg) => write!(f, "config validation error: {msg}"),
            ConfigError::ShellExpand(msg) => write!(f, "shell expansion error: {msg}"),
            ConfigError::AgentResolution(msg) => write!(f, "agent resolution error: {msg}"),
            ConfigError::EnvVar(msg) => write!(f, "environment variable error: {msg}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse(e) => Some(e),
            _ => None,
        }
    }
}

/// Verify that every agent referenced in workflow steps exists in the global configuration.
///
/// Returns `Ok(())` if all agent references are valid, or `Err(ConfigError::AgentResolution)`
/// with a descriptive message naming the step, workflow file, and missing agent.
pub fn resolve_agents(config: &Config, workflows: &[Workflow]) -> Result<(), ConfigError> {
    use std::collections::HashSet;

    let agent_names: HashSet<&str> = config.agents.iter().map(|a| a.name.as_str()).collect();

    for wf in workflows {
        for step in &wf.steps {
            if !agent_names.contains(step.agent.as_str()) {
                return Err(ConfigError::AgentResolution(format!(
                    "Step '{}' in workflow '{}' references unknown agent '{}'",
                    step.name, wf.path, step.agent
                )));
            }
        }
    }
    Ok(())
}

/// Validate that required environment variables are set based on the configured platform.
///
/// Checks globally required variables (`HERMES_API_KEY`) and
/// platform-specific variables (`GITHUB_TOKEN` for GitHub, `GITLAB_TOKEN` for GitLab).
/// Note: `WEBHOOK_SECRET` is not required as an env var because the webhook secret
/// can be provided via `config.toml` (`server.webhook_secret`). The env var is
/// optional and overrides the config value when set.
/// Returns `Ok(())` if all required variables are present, or `Err(ConfigError::EnvVar)`
/// with a descriptive message naming the first missing variable.
pub fn validate_env_vars(platform: &Platform) -> Result<(), ConfigError> {
    let required_globals = [env::HERMES_API_KEY];
    for var in required_globals {
        if std::env::var(var).is_err() {
            return Err(ConfigError::EnvVar(format!(
                "Missing required env var: {var}"
            )));
        }
    }

    match platform {
        Platform::Github => {
            if std::env::var("GITHUB_TOKEN").is_err() {
                return Err(ConfigError::EnvVar(
                    "Missing required env var: GITHUB_TOKEN".to_string(),
                ));
            }
        }
        Platform::Gitlab => {
            if std::env::var("GITLAB_TOKEN").is_err() {
                return Err(ConfigError::EnvVar(
                    "Missing required env var: GITLAB_TOKEN".to_string(),
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Mutex to serialize env-var tests that mutate global state.
    /// Without this, parallel test execution causes race conditions
    /// (e.g., one test removing HERMES_API_KEY while another expects it set).
    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    fn valid_toml() -> &'static str {
        r#"
platform = "github"

[[repos]]
owner = "example-corp"
repo = "backend-service"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"

[runtime]
max_concurrent = 2
workdir = "/tmp/.yoke"

[server]
host = "0.0.0.0"
webhook_host = "yoke.example.com"
port = 8644
webhook_secret = "secret"
max_body_size = 1048576
"#
    }

    #[test]
    fn test_load_valid_config() {
        let config = Config::from_str(valid_toml()).expect("should parse valid config");
        assert_eq!(config.platform, Platform::Github);
        assert_eq!(config.repos.len(), 1);
        assert_eq!(config.repos[0].owner, "example-corp");
        assert_eq!(config.repos[0].repo, "backend-service");
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.agents[0].name, "pm");
        assert_eq!(config.agents[1].name, "swe");
        assert_eq!(config.runtime.max_concurrent, 2);
        assert_eq!(config.runtime.workdir, "/tmp/.yoke");
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.webhook_host, "yoke.example.com");
        assert_eq!(config.server.port, 8644);
        assert_eq!(config.server.webhook_secret, "secret");
        assert_eq!(config.server.max_body_size, 1_048_576);
    }

    #[test]
    fn test_defaults() {
        let minimal = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let config = Config::from_str(minimal).expect("should parse minimal config");
        assert!(config.repos.is_empty());
        assert_eq!(config.runtime.max_concurrent, 0);
        assert!(!config.runtime.workdir.is_empty());
        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.webhook_host, "yoke.example.com");
        assert_eq!(config.server.port, 8644);
        assert_eq!(config.server.max_body_size, 1_048_576);
    }

    #[test]
    fn test_tilde_expansion() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[runtime]
workdir = "~/.yoke"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let config = Config::from_str(toml).expect("should parse config with tilde");
        assert!(
            !config.runtime.workdir.starts_with('~'),
            "workdir should have tilde expanded, got: {}",
            config.runtime.workdir
        );
        assert!(
            config.runtime.workdir.contains(".yoke"),
            "expanded workdir should still contain .yoke, got: {}",
            config.runtime.workdir
        );
    }

    #[test]
    fn test_missing_platform() {
        let toml = r#"
[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail without platform");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("platform"),
            "error should mention platform, got: {err_msg}"
        );
    }

    #[test]
    fn test_invalid_platform() {
        let toml = r#"
platform = "bitbucket"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail with invalid platform");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("bitbucket") || err_msg.contains("variant"),
            "error should mention the invalid value or variant, got: {err_msg}"
        );
    }

    #[test]
    fn test_invalid_toml_syntax() {
        let toml = r#"
platform = "github"
this is not valid toml [[[
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail with broken TOML syntax");
        match result.unwrap_err() {
            ConfigError::Parse(_) => {} // expected
            other => panic!("expected Parse error, got: {other}"),
        }
    }

    #[test]
    fn test_invalid_url() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "not-a-url"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail with invalid URL");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("base_url") || err_msg.contains("URL") || err_msg.contains("url"),
            "error should mention URL issue, got: {err_msg}"
        );
    }

    #[test]
    fn test_missing_webhook_secret() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
host = "0.0.0.0"
port = 8644
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail without webhook_secret");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("webhook_secret"),
            "error should mention webhook_secret, got: {err_msg}"
        );
    }

    #[test]
    fn test_missing_webhook_host() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail without webhook_host");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("webhook_host"),
            "error should mention webhook_host, got: {err_msg}"
        );
    }

    #[test]
    fn test_gitlab_platform() {
        let toml = r#"
platform = "gitlab"
gitlab_url = "https://gitlab.mycompany.com"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "gitlab-token"
"#;
        let config = Config::from_str(toml).expect("should parse gitlab config");
        assert_eq!(config.platform, Platform::Gitlab);
        assert!(config.gitlab_url.is_some());
        assert_eq!(
            config.gitlab_url.unwrap().as_str(),
            "https://gitlab.mycompany.com/"
        );
    }

    #[test]
    fn test_gitlab_config_section() {
        let toml = r#"
platform = "gitlab"

[gitlab]
gitlab_url = "https://gitlab.example.com"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "token"
"#;
        let config = Config::from_str(toml).expect("should parse gitlab section");
        assert_eq!(config.platform, Platform::Gitlab);
        assert!(config.gitlab.is_some());
        assert_eq!(
            config.gitlab.unwrap().gitlab_url.as_str(),
            "https://gitlab.example.com/"
        );
    }

    #[test]
    fn test_missing_agents() {
        let toml = r#"
platform = "github"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail without agents");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("agent"),
            "error should mention agents, got: {err_msg}"
        );
    }

    #[test]
    fn test_duplicate_agent_names() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "pm"
base_url = "http://localhost:8001"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail with duplicate agent names");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("duplicate") || err_msg.contains("pm"),
            "error should mention duplicate agent name, got: {err_msg}"
        );
    }

    #[test]
    fn test_empty_repos_is_valid() {
        let toml = r#"
platform = "github"
repos = []

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let config = Config::from_str(toml).expect("empty repos is valid");
        assert!(config.repos.is_empty());
    }

    #[test]
    fn test_multiple_repos() {
        let toml = r#"
platform = "github"

[[repos]]
owner = "org1"
repo = "repo1"

[[repos]]
owner = "org2"
repo = "repo2"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let config = Config::from_str(toml).expect("should parse multiple repos");
        assert_eq!(config.repos.len(), 2);
    }

    #[test]
    fn test_file_not_found() {
        let result = Config::load("/nonexistent/path/config.toml");
        assert!(result.is_err(), "should fail for missing file");
        match result.unwrap_err() {
            ConfigError::Io(_) => {} // expected
            other => panic!("expected Io error, got: {other}"),
        }
    }

    #[test]
    fn test_invalid_url_scheme() {
        let toml = r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "ftp://localhost:8000"

[server]
webhook_host = "yoke.example.com"
webhook_secret = "secret"
"#;
        let result = Config::from_str(toml);
        assert!(result.is_err(), "should fail with non-http URL scheme");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("scheme") || err_msg.contains("URL") || err_msg.contains("http"),
            "error should mention invalid scheme, got: {err_msg}"
        );
    }

    #[test]
    fn test_config_error_display() {
        let err = ConfigError::Validation("test error".to_string());
        assert_eq!(err.to_string(), "config validation error: test error");

        let err = ConfigError::ShellExpand("bad tilde".to_string());
        assert_eq!(err.to_string(), "shell expansion error: bad tilde");

        let err = ConfigError::AgentResolution("missing agent".to_string());
        assert_eq!(err.to_string(), "agent resolution error: missing agent");

        let err = ConfigError::EnvVar("Missing required env var: GITHUB_TOKEN".to_string());
        assert_eq!(
            err.to_string(),
            "environment variable error: Missing required env var: GITHUB_TOKEN"
        );
    }

    // --- Agent resolution tests ---

    fn make_config(agent_names: &[&str]) -> Config {
        Config {
            platform: Platform::Github,
            repos: vec![],
            agents: agent_names
                .iter()
                .map(|name| AgentConfig {
                    name: name.to_string(),
                    base_url: Url::parse("http://localhost:8000").unwrap(),
                })
                .collect(),
            runtime: RuntimeConfig::default(),
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                webhook_host: "yoke.example.com".to_string(),
                port: 8644,
                webhook_secret: "secret".to_string(),
                max_body_size: 1_048_576,
            },
            github: None,
            gitlab: None,
            gitlab_url: None,
        }
    }

    fn make_workflow(path: &str, steps: Vec<(&str, &str)>) -> Workflow {
        Workflow {
            path: path.to_string(),
            trigger: crate::workflow::Trigger {
                r#type: crate::workflow::triggers::GITHUB_ISSUE_ASSIGNED.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            },
            git: crate::workflow::GitConfig::default(),
            steps: steps
                .into_iter()
                .map(|(name, agent)| crate::workflow::Step {
                    name: name.to_string(),
                    agent: agent.to_string(),
                    prompt_template: "Do something".to_string(),
                    pre_hooks: vec![],
                    post_hooks: vec![],
                })
                .collect(),
        }
    }

    #[test]
    fn test_resolve_agents_success() {
        let config = make_config(&["pm", "swe"]);
        let workflows = vec![make_workflow(
            "flows/plan.toml",
            vec![("Plan", "pm"), ("Implement", "swe")],
        )];
        assert!(resolve_agents(&config, &workflows).is_ok());
    }

    #[test]
    fn test_resolve_agents_empty_workflows() {
        let config = make_config(&["pm"]);
        let workflows: Vec<Workflow> = vec![];
        assert!(resolve_agents(&config, &workflows).is_ok());
    }

    #[test]
    fn test_resolve_agents_missing_agent() {
        let config = make_config(&["pm"]);
        let workflows = vec![make_workflow("flows/dev.toml", vec![("Code", "swe")])];
        let result = resolve_agents(&config, &workflows);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(
                "Step 'Code' in workflow 'flows/dev.toml' references unknown agent 'swe'"
            ),
            "unexpected error message: {err_msg}"
        );
    }

    #[test]
    fn test_resolve_agents_multiple_workflows_first_error() {
        let config = make_config(&["pm"]);
        let workflows = vec![
            make_workflow("flows/plan.toml", vec![("Plan", "pm")]),
            make_workflow("flows/dev.toml", vec![("Code", "swe")]),
        ];
        let result = resolve_agents(&config, &workflows);
        assert!(result.is_err());
        // Should fail on the second workflow's unknown agent
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("unknown agent 'swe'"),
            "unexpected error message: {err_msg}"
        );
    }

    #[test]
    fn test_resolve_agents_all_agents_known_across_workflows() {
        let config = make_config(&["pm", "swe", "reviewer"]);
        let workflows = vec![
            make_workflow("flows/plan.toml", vec![("Plan", "pm")]),
            make_workflow(
                "flows/dev.toml",
                vec![("Code", "swe"), ("Review", "reviewer")],
            ),
        ];
        assert!(resolve_agents(&config, &workflows).is_ok());
    }

    // --- Environment variable validation tests ---

    #[test]
    fn test_validate_env_vars_missing_hermes_api_key() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::remove_var(env::HERMES_API_KEY);
            std::env::remove_var(env::GITHUB_TOKEN);
        }

        let result = validate_env_vars(&Platform::Github);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(env::HERMES_API_KEY),
            "error should mention HERMES_API_KEY, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_env_vars_webhook_secret_not_required() {
        // WEBHOOK_SECRET is no longer required by validate_env_vars;
        // it can be provided via config.toml instead.
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(env::HERMES_API_KEY, "test-key");
            std::env::set_var(env::GITHUB_TOKEN, "gh-token");
            std::env::remove_var(env::WEBHOOK_SECRET);
        }

        let result = validate_env_vars(&Platform::Github);
        assert!(
            result.is_ok(),
            "WEBHOOK_SECRET should be optional: {:?}",
            result
        );
    }

    #[test]
    fn test_validate_env_vars_github_token_missing() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(env::HERMES_API_KEY, "valid");
            std::env::set_var(env::WEBHOOK_SECRET, "valid");
            std::env::remove_var(env::GITHUB_TOKEN);
        }

        let result = validate_env_vars(&Platform::Github);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(env::GITHUB_TOKEN),
            "error should mention GITHUB_TOKEN, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_env_vars_gitlab_token_missing() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(env::HERMES_API_KEY, "valid");
            std::env::set_var(env::WEBHOOK_SECRET, "valid");
            std::env::remove_var(env::GITLAB_TOKEN);
        }

        let result = validate_env_vars(&Platform::Gitlab);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains(env::GITLAB_TOKEN),
            "error should mention GITLAB_TOKEN, got: {err_msg}"
        );
    }

    #[test]
    fn test_validate_env_vars_github_all_present() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(env::HERMES_API_KEY, "valid");
            std::env::set_var(env::WEBHOOK_SECRET, "valid");
            std::env::set_var(env::GITHUB_TOKEN, "gh-token");
        }

        let result = validate_env_vars(&Platform::Github);
        assert!(result.is_ok(), "should succeed with all vars set");
    }

    #[test]
    fn test_validate_env_vars_gitlab_all_present() {
        let _guard = ENV_MUTEX.lock().unwrap();
        unsafe {
            std::env::set_var(env::HERMES_API_KEY, "valid");
            std::env::set_var(env::WEBHOOK_SECRET, "valid");
            std::env::set_var(env::GITLAB_TOKEN, "gl-token");
        }

        let result = validate_env_vars(&Platform::Gitlab);
        assert!(result.is_ok(), "should succeed with all vars set");
    }
}
