//! Webhook management module — unified interface for GitHub and GitLab webhook operations.
//!
//! This module provides:
//! - [`WebhookInfo`] — shared type representing a webhook across platforms
//! - [`WebhookConfig`] — shared configuration for creating/updating webhooks
//! - [`WebhookClient`] — enum-based dispatcher selecting the right platform implementation
//! - [`GitHubWebhookClient`] — GitHub implementation using [`crate::github_api::GitHubClient`]
//! - [`GitLabWebhookClient`] — GitLab implementation using [`crate::webhook::gitlab_api::GitLabClient`]

use crate::config::Platform;
use crate::github_api;
use crate::webhook::gitlab_api;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Shared types
// ---------------------------------------------------------------------------

/// A platform-agnostic representation of a webhook.
///
/// Maps from platform-specific types (GitHub `Webhook`, GitLab `GitLabWebhook`)
/// so that CLI handlers can work with a uniform type regardless of platform.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookInfo {
    pub id: u64,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub events: Vec<String>,
    pub active: bool,
}

/// Configuration for creating or updating a webhook.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub events: Vec<String>,
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur during webhook management operations.
#[derive(Error, Debug)]
pub enum WebhookError {
    /// An error from the GitHub API client.
    #[error("GitHub API error: {0}")]
    GitHub(#[from] github_api::GitHubError),
    /// An error from the GitLab API client.
    #[error("GitLab API error: {0}")]
    GitLab(#[from] gitlab_api::GitLabError),
    /// An error due to missing or invalid configuration.
    #[error("Configuration error: {0}")]
    Config(String),
}

// ---------------------------------------------------------------------------
// GitHub client wrapper
// ---------------------------------------------------------------------------

/// GitHub webhook client that wraps [`github_api::GitHubClient`].
///
/// Adapts the GitHub-specific API to uniform [`WebhookInfo`] / [`WebhookConfig`] types.
#[derive(Debug)]
pub struct GitHubWebhookClient {
    client: github_api::GitHubClient,
    owner: String,
}

impl GitHubWebhookClient {
    /// Create a new GitHub webhook client.
    ///
    /// `owner` is the GitHub user/organization name that owns the repositories.
    /// The GitHub token is passed directly (typically from the `GITHUB_TOKEN` env var).
    pub fn new(token: String, owner: String) -> Self {
        Self {
            client: github_api::GitHubClient::new(token, None),
            owner,
        }
    }

    /// List all webhooks for a repository.
    pub async fn list_webhooks(&self, repo: &str) -> Result<Vec<WebhookInfo>, WebhookError> {
        let webhooks = self.client.list_webhooks(&self.owner, repo).await?;
        Ok(webhooks.into_iter().map(WebhookInfo::from).collect())
    }

    /// Create a new webhook for a repository.
    pub async fn create_webhook(
        &self,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        let gh_config = github_api::WebhookConfig {
            url: config.url.clone(),
            secret: config.secret.clone().unwrap_or_default(),
            events: config.events.clone(),
        };
        let webhook = self
            .client
            .create_webhook(&self.owner, repo, &gh_config)
            .await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Update an existing webhook by ID.
    pub async fn update_webhook(
        &self,
        repo: &str,
        id: u64,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        let gh_config = github_api::WebhookConfig {
            url: config.url.clone(),
            secret: config.secret.clone().unwrap_or_default(),
            events: config.events.clone(),
        };
        let webhook = self
            .client
            .update_webhook(&self.owner, repo, id, &gh_config)
            .await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(&self, repo: &str, id: u64) -> Result<(), WebhookError> {
        self.client.delete_webhook(&self.owner, repo, id).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitLab client wrapper
// ---------------------------------------------------------------------------

/// GitLab webhook client that wraps [`gitlab_api::GitLabClient`].
///
/// Adapts the GitLab-specific API to uniform [`WebhookInfo`] / [`WebhookConfig`] types.
#[derive(Debug)]
pub struct GitLabWebhookClient {
    client: gitlab_api::GitLabClient,
}

impl GitLabWebhookClient {
    /// Create a new GitLab webhook client.
    ///
    /// `base_url` overrides the default `https://gitlab.com/api/v4` for self-hosted instances.
    pub fn new(token: String, base_url: Option<String>) -> Self {
        Self {
            client: gitlab_api::GitLabClient::new(token, base_url),
        }
    }

    /// List all webhooks for a project.
    pub async fn list_webhooks(&self, project_id: &str) -> Result<Vec<WebhookInfo>, WebhookError> {
        let webhooks = self.client.list_webhooks(project_id).await?;
        Ok(webhooks.into_iter().map(WebhookInfo::from).collect())
    }

    /// Create a new webhook for a project.
    pub async fn create_webhook(
        &self,
        project_id: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        let gl_config = gitlab_api::WebhookConfig {
            url: config.url.clone(),
            token: config.secret.clone(),
            push_disabled: None,
            active: Some(true),
            events: Some(config.events.clone()),
        };
        let webhook = self.client.create_webhook(project_id, &gl_config).await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Update an existing webhook by ID.
    pub async fn update_webhook(
        &self,
        project_id: &str,
        id: u64,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        let gl_config = gitlab_api::WebhookConfig {
            url: config.url.clone(),
            token: config.secret.clone(),
            push_disabled: None,
            active: Some(true),
            events: Some(config.events.clone()),
        };
        let webhook = self
            .client
            .update_webhook(project_id, id, &gl_config)
            .await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(&self, project_id: &str, id: u64) -> Result<(), WebhookError> {
        self.client.delete_webhook(project_id, id).await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

impl From<github_api::Webhook> for WebhookInfo {
    fn from(w: github_api::Webhook) -> Self {
        Self {
            id: w.id,
            url: w.url,
            secret: w.secret,
            events: w.events,
            active: w.active,
        }
    }
}

impl From<gitlab_api::GitLabWebhook> for WebhookInfo {
    fn from(w: gitlab_api::GitLabWebhook) -> Self {
        Self {
            id: w.id,
            url: w.url,
            // GitLab does not expose the secret in list responses
            secret: None,
            events: Vec::new(), // GitLab webhook response doesn't carry events list
            active: w.active,
        }
    }
}

// ---------------------------------------------------------------------------
// Platform dispatcher (enum-based)
// ---------------------------------------------------------------------------

/// Platform-dispatching webhook client.
///
/// Wraps either a `GitHubWebhookClient` or `GitLabWebhookClient` and
/// delegates operations to the correct implementation based on the
/// configured platform.
#[derive(Debug)]
pub enum WebhookClient {
    Github(GitHubWebhookClient),
    Gitlab(GitLabWebhookClient),
}

impl WebhookClient {
    /// Create the appropriate `WebhookClient` variant for the given platform.
    ///
    /// Reads platform-specific tokens from environment variables:
    /// - GitHub: `GITHUB_TOKEN`
    /// - GitLab: `GITLAB_TOKEN`
    ///
    /// Returns a `WebhookError::Config` if the required token is missing.
    pub fn new(
        platform: &Platform,
        owner: &str,
        gitlab_url: Option<String>,
    ) -> Result<Self, WebhookError> {
        match platform {
            Platform::Github => {
                let token = std::env::var("GITHUB_TOKEN").map_err(|_| {
                    WebhookError::Config("Missing required env var: GITHUB_TOKEN".to_string())
                })?;
                Ok(WebhookClient::Github(GitHubWebhookClient::new(
                    token,
                    owner.to_string(),
                )))
            }
            Platform::Gitlab => {
                let token = std::env::var("GITLAB_TOKEN").map_err(|_| {
                    WebhookError::Config("Missing required env var: GITLAB_TOKEN".to_string())
                })?;
                Ok(WebhookClient::Gitlab(GitLabWebhookClient::new(
                    token, gitlab_url,
                )))
            }
        }
    }

    /// List all webhooks for a repository/project.
    pub async fn list_webhooks(&self, repo: &str) -> Result<Vec<WebhookInfo>, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.list_webhooks(repo).await,
            WebhookClient::Gitlab(c) => c.list_webhooks(repo).await,
        }
    }

    /// Create a new webhook.
    pub async fn create_webhook(
        &self,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.create_webhook(repo, config).await,
            WebhookClient::Gitlab(c) => c.create_webhook(repo, config).await,
        }
    }

    /// Update an existing webhook by ID.
    pub async fn update_webhook(
        &self,
        repo: &str,
        id: u64,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.update_webhook(repo, id, config).await,
            WebhookClient::Gitlab(c) => c.update_webhook(repo, id, config).await,
        }
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(&self, repo: &str, id: u64) -> Result<(), WebhookError> {
        match self {
            WebhookClient::Github(c) => c.delete_webhook(repo, id).await,
            WebhookClient::Gitlab(c) => c.delete_webhook(repo, id).await,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_webhook_info_from_github() {
        let gh = github_api::Webhook {
            id: 123,
            url: "https://example.com/hook".to_string(),
            secret: Some("s3cret".to_string()),
            events: vec!["push".to_string(), "pull_request".to_string()],
            active: true,
        };
        let info = WebhookInfo::from(gh);
        assert_eq!(info.id, 123);
        assert_eq!(info.url, "https://example.com/hook");
        assert_eq!(info.secret, Some("s3cret".to_string()));
        assert_eq!(info.events, vec!["push", "pull_request"]);
        assert!(info.active);
    }

    #[test]
    fn test_webhook_info_from_gitlab() {
        let gl = gitlab_api::GitLabWebhook {
            id: 456,
            url: "https://example.com/gl-hook".to_string(),
            push_disabled: false,
            active: true,
        };
        let info = WebhookInfo::from(gl);
        assert_eq!(info.id, 456);
        assert_eq!(info.url, "https://example.com/gl-hook");
        assert!(info.secret.is_none());
        assert!(info.events.is_empty());
        assert!(info.active);
    }

    #[test]
    fn test_webhook_config_serialization() {
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            secret: Some("my-secret".to_string()),
            events: vec!["push".to_string()],
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["url"], "https://example.com/hook");
        assert_eq!(json["secret"], "my-secret");
        assert_eq!(json["events"][0], "push");
    }

    #[test]
    fn test_webhook_config_serialization_no_secret() {
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            secret: None,
            events: vec!["push".to_string()],
        };
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("secret").is_none());
    }

    #[test]
    fn test_webhook_client_new_github_missing_token() {
        unsafe { std::env::remove_var("GITHUB_TOKEN") };
        let result = WebhookClient::new(&Platform::Github, "owner", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("GITHUB_TOKEN"),
            "Error should mention GITHUB_TOKEN, got: {err}"
        );
    }

    #[test]
    fn test_webhook_client_new_gitlab_missing_token() {
        unsafe { std::env::remove_var("GITLAB_TOKEN") };
        let result = WebhookClient::new(&Platform::Gitlab, "owner", None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("GITLAB_TOKEN"),
            "Error should mention GITLAB_TOKEN, got: {err}"
        );
    }

    #[test]
    fn test_error_display_github() {
        let err = WebhookError::Config("test error".to_string());
        assert_eq!(err.to_string(), "Configuration error: test error");
    }
}
