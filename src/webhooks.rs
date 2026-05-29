//! Webhook management module — unified interface for GitHub and GitLab webhook operations.
//!
//! This module provides:
//! - [`WebhookInfo`] — shared type representing a webhook across platforms
//! - [`WebhookConfig`] — shared configuration for creating/updating webhooks
//! - [`WebhookClient`] — enum-based dispatcher selecting the right platform implementation
//! - [`GitHubWebhookClient`] — GitHub implementation using [`crate::github_api::GitHubClient`]
//! - [`GitLabWebhookClient`] — GitLab implementation using [`crate::webhook::gitlab_api::GitLabClient`]
//! - [`webhooks_list`] — list webhooks for all configured repositories
//! - [`webhooks_remove`] — remove webhooks matching Yoke's URL for all configured repositories
//! - [`webhooks_add`] — idempotently create or update webhooks for all configured repositories

use crate::config::{Config, Platform};
use crate::github_api;
use crate::webhook::gitlab_api;
use crate::workflow;
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::path::Path;
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

impl Display for WebhookConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let secret = match self.secret {
            Some(_) => "<hidden>",
            None => "<empty>",
        };

        write!(
            f,
            "{{ url: {}, secret: {}, events: {} }}",
            self.url,
            secret,
            self.events.join(", ")
        )
    }
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
/// Owner is passed per-operation to support repos across multiple owners/orgs.
#[derive(Debug)]
pub struct GitHubWebhookClient {
    client: github_api::GitHubClient,
}

impl GitHubWebhookClient {
    /// Create a new GitHub webhook client.
    ///
    /// The GitHub token is passed directly (typically from the `GITHUB_TOKEN` env var).
    /// Owner is passed per-operation to support repos across multiple owners/orgs.
    pub fn new(token: String) -> Self {
        Self {
            client: github_api::GitHubClient::new(token, None),
        }
    }

    /// Create a new GitHub webhook client with a custom API base URL.
    ///
    /// Useful for testing against mock servers. `base_url` replaces the
    /// default `https://api.github.com`.
    pub fn new_with_base_url(token: String, base_url: String) -> Self {
        Self {
            client: github_api::GitHubClient::new(token, Some(base_url)),
        }
    }

    /// List all webhooks for a repository.
    pub async fn list_webhooks(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<WebhookInfo>, WebhookError> {
        let webhooks = self.client.list_webhooks(owner, repo).await?;
        Ok(webhooks.into_iter().map(WebhookInfo::from).collect())
    }

    /// Create a new webhook for a repository.
    pub async fn create_webhook(
        &self,
        owner: &str,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        let gh_config = github_api::WebhookConfig {
            url: config.url.clone(),
            secret: config.secret.clone().unwrap_or_default(),
            events: config.events.clone(),
        };
        let webhook = self.client.create_webhook(owner, repo, &gh_config).await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Update an existing webhook by ID.
    pub async fn update_webhook(
        &self,
        owner: &str,
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
            .update_webhook(owner, repo, id, &gh_config)
            .await?;
        Ok(WebhookInfo::from(webhook))
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(
        &self,
        owner: &str,
        repo: &str,
        id: u64,
    ) -> Result<(), WebhookError> {
        self.client.delete_webhook(owner, repo, id).await?;
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
        _owner: &str, // Kept for backward compat, no longer used for GitHub
        gitlab_url: Option<String>,
    ) -> Result<Self, WebhookError> {
        match platform {
            Platform::Github => {
                let token = std::env::var("GITHUB_TOKEN").map_err(|_| {
                    WebhookError::Config("Missing required env var: GITHUB_TOKEN".to_string())
                })?;
                Ok(WebhookClient::Github(GitHubWebhookClient::new(token)))
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
    pub async fn list_webhooks(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<WebhookInfo>, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.list_webhooks(owner, repo).await,
            WebhookClient::Gitlab(c) => c.list_webhooks(repo).await,
        }
    }

    /// Create a new webhook.
    pub async fn create_webhook(
        &self,
        owner: &str,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.create_webhook(owner, repo, config).await,
            WebhookClient::Gitlab(c) => c.create_webhook(repo, config).await,
        }
    }

    /// Update an existing webhook by ID.
    pub async fn update_webhook(
        &self,
        owner: &str,
        repo: &str,
        id: u64,
        config: &WebhookConfig,
    ) -> Result<WebhookInfo, WebhookError> {
        match self {
            WebhookClient::Github(c) => c.update_webhook(owner, repo, id, config).await,
            WebhookClient::Gitlab(c) => c.update_webhook(repo, id, config).await,
        }
    }

    /// Delete a webhook by ID.
    pub async fn delete_webhook(
        &self,
        owner: &str,
        repo: &str,
        id: u64,
    ) -> Result<(), WebhookError> {
        match self {
            WebhookClient::Github(c) => c.delete_webhook(owner, repo, id).await,
            WebhookClient::Gitlab(c) => c.delete_webhook(repo, id).await,
        }
    }
}

// ---------------------------------------------------------------------------
// High-level command handlers
// ---------------------------------------------------------------------------

/// Summary counters returned by [`webhooks_add`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AddSummary {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
    pub errors: usize,
}

/// Summary counters returned by [`webhooks_remove`].
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RemoveSummary {
    pub deleted: usize,
    pub not_found: usize,
    pub errors: usize,
}

/// Construct the Yoke webhook URL from config server settings.
///
/// Uses `webhook_host` (the external hostname) rather than `host` (the bind
/// address) so that the registered webhook URL resolves from the internet.
/// Format: `https://{webhook_host}/webhook`
fn yoke_webhook_url(config: &Config) -> String {
    format!("https://{}/webhook", config.server.webhook_host)
}

/// List all webhooks for configured repositories in a human-readable table.
///
/// For each repository, fetches webhooks via the platform client and prints:
/// `ID | URL | Secret (last 4) | Events | Active`.
/// Webhooks whose URL matches Yoke's configured webhook URL are marked with
/// a `(yoke)` label.
pub async fn webhooks_list(config: &Config, client: &WebhookClient) -> Result<(), WebhookError> {
    let yoke_url = yoke_webhook_url(config);

    for repo in &config.repos {
        let repo_display = format!("{}/{}", repo.owner, repo.repo);
        match client.list_webhooks(&repo.owner, &repo.repo).await {
            Ok(hooks) => {
                tracing::info!(repo = %repo_display, "Listing webhooks");
                if hooks.is_empty() {
                    tracing::info!("No webhooks found");
                }
                for hook in hooks {
                    let yoke_tag = if hook.url == yoke_url { " (yoke)" } else { "" };
                    tracing::info!(
                        id = hook.id,
                        url = %hook.url,
                        yoke = !yoke_tag.is_empty(),
                        events = ?hook.events,
                        active = hook.active,
                        "Webhook found"
                    );
                }
            }
            Err(e) => {
                tracing::error!(repo = %repo_display, error = %e, "Error listing webhooks");
            }
        }
    }
    Ok(())
}

/// Remove all webhooks matching Yoke's URL from configured repositories.
///
/// For each repository, lists existing webhooks, finds those whose URL
/// matches `https://{host}:{port}/webhook`, and deletes them.
/// Returns a [`RemoveSummary`] with deleted, not_found, and error counts.
pub async fn webhooks_remove(
    config: &Config,
    client: &WebhookClient,
) -> Result<RemoveSummary, WebhookError> {
    let yoke_url = yoke_webhook_url(config);
    let mut summary = RemoveSummary::default();

    for repo in &config.repos {
        let repo_display = format!("{}/{}", repo.owner, repo.repo);
        let hooks = match client.list_webhooks(&repo.owner, &repo.repo).await {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(repo = %repo_display, error = %e, "Error listing webhooks");
                summary.errors += 1;
                continue;
            }
        };

        let matching: Vec<&WebhookInfo> = hooks.iter().filter(|h| h.url == yoke_url).collect();
        if matching.is_empty() {
            tracing::info!(repo = %repo_display, "No Yoke webhooks found");
            summary.not_found += 1;
            continue;
        }

        for hook in &matching {
            match client
                .delete_webhook(&repo.owner, &repo.repo, hook.id)
                .await
            {
                Ok(()) => {
                    tracing::info!(repo = %repo_display, id = hook.id, url = %hook.url, "Deleted webhook");
                    summary.deleted += 1;
                }
                Err(e) => {
                    tracing::error!(repo = %repo_display, webhook_id = hook.id, error = %e, "Error deleting webhook");
                    summary.errors += 1;
                }
            }
        }
    }

    tracing::info!(
        deleted = summary.deleted,
        not_found = summary.not_found,
        errors = summary.errors,
        "Webhook removal complete"
    );
    Ok(summary)
}

/// Idempotently create or update webhooks for all configured repositories.
///
/// Loads workflows from `workflows_path`, derives the required event
/// subscriptions, and for each repository either creates a new webhook
/// or updates an existing one (matched by URL).
///
/// Returns an [`AddSummary`] with created, updated, skipped, and error counts.
pub async fn webhooks_add(
    config: &Config,
    client: &WebhookClient,
    workflows_path: &Path,
) -> Result<AddSummary, WebhookError> {
    let yoke_url = yoke_webhook_url(config);

    // Load workflows and derive required events
    let workflows = workflow::load_workflows(workflows_path).map_err(|e| {
        WebhookError::Config(format!(
            "Failed to load workflows from {}: {e}",
            workflows_path.display()
        ))
    })?;
    let workflow_refs: Vec<workflow::Workflow> = workflows.iter().map(|(_, w)| w.clone()).collect();
    let events = workflow::derive_required_events(&workflow_refs);

    if events.is_empty() {
        tracing::warn!("No workflow triggers found; subscribing to no events");
    }

    let hook_config = WebhookConfig {
        url: yoke_url.clone(),
        secret: Some(config.server.webhook_secret.clone()),
        events,
    };

    let mut summary = AddSummary::default();

    for repo in &config.repos {
        let repo_display = format!("{}/{}", repo.owner, repo.repo);
        let existing = match client.list_webhooks(&repo.owner, &repo.repo).await {
            Ok(hooks) => hooks,
            Err(e) => {
                tracing::error!(repo = %repo_display, error = %e, "Error listing webhooks");
                summary.errors += 1;
                continue;
            }
        };

        let matched = existing.iter().find(|h| h.url == yoke_url);
        match matched {
            Some(hook) => {
                match client
                    .update_webhook(&repo.owner, &repo.repo, hook.id, &hook_config)
                    .await
                {
                    Ok(_) => {
                        tracing::info!(repo = %repo_display, id = hook.id, url = %yoke_url, "Updated webhook");
                        summary.updated += 1;
                    }
                    Err(e) => {
                        tracing::error!(repo = %repo_display, webhook_id = hook.id, error = %e, "Error updating webhook");
                        summary.errors += 1;
                    }
                }
            }
            None => match client
                .create_webhook(&repo.owner, &repo.repo, &hook_config)
                .await
            {
                Ok(hook) => {
                    tracing::info!(repo = %repo_display, id = hook.id, url = %yoke_url, "Created webhook");
                    summary.created += 1;
                }
                Err(e) => {
                    tracing::error!(repo = %repo_display, error = %e, hook_config = %hook_config, "Error creating webhook");
                    summary.errors += 1;
                }
            },
        }
    }

    tracing::info!(
        created = summary.created,
        updated = summary.updated,
        skipped = summary.skipped,
        errors = summary.errors,
        "Webhook setup complete"
    );
    Ok(summary)
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
