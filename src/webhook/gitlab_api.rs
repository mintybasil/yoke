//! GitLab REST API client for project webhook and event operations.
//!
//! This module provides a [`GitLabClient`] that wraps [`reqwest::Client`] and
//! handles Private-Token authentication, error mapping, and pagination for
//! GitLab's project webhook endpoints. It mirrors the structure of
//! [`crate::webhook::github_api::GitHubClient`].
//!
//! The [`ProjectEvent`] struct and [`ProjectEvent::try_into_trigger_event`]
//! method support the catch-up flow: fetching missed events from GitLab's
//! Project Events API and mapping them into Yoke's internal [`TriggerEvent`]
//! format.

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by the GitLab REST API client.
#[derive(Error, Debug)]
pub enum GitLabError {
    /// The HTTP request itself failed (network error, timeout, etc.).
    #[error("HTTP request failed: {0}")]
    RequestError(#[from] reqwest::Error),
    /// Authentication failed (HTTP 401).
    #[error("Authentication failed (401)")]
    Unauthorized,
    /// The requested resource was not found (HTTP 404).
    #[error("Resource not found (404)")]
    NotFound,
    /// Any other API error with an HTTP status code.
    #[error("API error: {0}")]
    ApiError(String),
}

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Project Events API models (catch-up)
// ---------------------------------------------------------------------------

/// A single event from the GitLab Project Events API
/// (`GET /projects/{id}/events`).
///
/// This struct deserializes the event list response used for catching up on
/// missed events. The `action_name` and `target_type` fields together
/// determine the [`TriggerType`] mapping in
/// [`ProjectEvent::try_into_trigger_event`].
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectEvent {
    pub id: u64,
    /// The action that triggered this event (e.g. "pushed_to", "opened", "merged").
    #[serde(rename = "action_name")]
    pub action_name: String,
    /// When the event was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// The user who performed the action (the "actor").
    pub author: crate::webhook::gitlab::GitLabUser,
    /// Push-specific data. Present only for push events.
    #[serde(default)]
    pub push_data: Option<ProjectPushData>,
    /// The type of target object (e.g. "MergeRequest", "Issue").
    #[serde(default)]
    pub target_type: Option<String>,
    /// The numeric ID of the target object (e.g. MR IID, issue IID).
    #[serde(default)]
    pub target_id: Option<u64>,
}

/// Push-specific data included in project events with `action_name == "pushed_to"`.
#[derive(Debug, Clone, Deserialize)]
pub struct ProjectPushData {
    /// The full SHA of the pushed commit.
    pub commit_sha: String,
    /// The branch the push was made to.
    pub branch: String,
    /// The number of commits in the push.
    #[serde(default)]
    pub commit_count: u64,
}

// ---------------------------------------------------------------------------
// Webhook models
// ---------------------------------------------------------------------------

/// A GitLab project webhook.
///
/// Only the fields needed for idempotency checks are deserialized;
/// everything else is ignored.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct GitLabWebhook {
    pub id: u64,
    pub url: String,
    #[serde(default)]
    pub push_disabled: bool,
    #[serde(default)]
    pub active: bool,
}

/// Configuration for creating or updating a GitLab project webhook.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookConfig {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub push_disabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// REST API client for GitLab.
///
/// Wraps a [`reqwest::Client`] and handles Private-Token authentication
/// headers, error mapping, and pagination transparently.
#[derive(Debug, Clone)]
pub struct GitLabClient {
    token: String,
    base_url: String,
    client: Client,
}

impl GitLabClient {
    /// Create a new client.
    ///
    /// If `base_url` is `None`, defaults to `https://gitlab.com/api/v4`.
    /// For self-hosted instances, pass the full API base URL
    /// (e.g. `https://gitlab.mycompany.com/api/v4`).
    pub fn new(token: String, base_url: Option<String>) -> Self {
        Self {
            token,
            base_url: base_url.unwrap_or_else(|| "https://gitlab.com/api/v4".to_string()),
            client: Client::new(),
        }
    }

    // -- Internal helpers ----------------------------------------------------

    /// Build the authentication + user-agent headers that every request needs.
    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "PRIVATE-TOKEN",
            self.token.parse().expect("header value should be valid"),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            "yoke-agent".parse().expect("header value should be valid"),
        );
        headers
    }

    /// Map an HTTP status code to a [`GitLabError`].
    fn map_status(status: reqwest::StatusCode) -> GitLabError {
        match status {
            reqwest::StatusCode::UNAUTHORIZED => GitLabError::Unauthorized,
            reqwest::StatusCode::NOT_FOUND => GitLabError::NotFound,
            other => GitLabError::ApiError(format!("Unexpected status: {other}")),
        }
    }

    /// Parse the `Link` response header and return the URL of the "next" page,
    /// if any.
    fn parse_next_link(headers: &reqwest::header::HeaderMap) -> Option<String> {
        let link = headers.get("link")?.to_str().ok()?;
        link.split(',')
            .find(|part| part.contains(r#"rel="next""#))
            .and_then(|part| {
                let start = part.find('<')? + 1;
                let end = part.find('>')?;
                Some(part[start..end].to_string())
            })
    }

    // -- Public API ----------------------------------------------------------

    /// List all webhooks for the given project.
    ///
    /// Handles pagination transparently — iterates until the GitLab API
    /// no longer returns a `rel="next"` Link header.
    ///
    /// `project_id` can be the numeric project ID or the URL-encoded
    /// `namespace/project` path (e.g. `"group%2Fproject"`).
    pub async fn list_webhooks(&self, project_id: &str) -> Result<Vec<GitLabWebhook>, GitLabError> {
        let mut all_webhooks = Vec::new();
        let mut next_url: Option<String> = Some(format!(
            "{}/projects/{}/webhooks",
            self.base_url, project_id
        ));

        while let Some(url) = next_url {
            let response = self
                .client
                .get(&url)
                .headers(self.auth_headers())
                .send()
                .await?;

            if response.status() != reqwest::StatusCode::OK {
                return Err(Self::map_status(response.status()));
            }

            // Capture pagination before consuming the body.
            next_url = Self::parse_next_link(response.headers());

            let page: Vec<GitLabWebhook> = response.json().await?;
            all_webhooks.extend(page);
        }

        Ok(all_webhooks)
    }

    /// Create a new webhook for the given project.
    ///
    /// `project_id` can be the numeric project ID or the URL-encoded
    /// `namespace/project` path (e.g. `"group%2Fproject"`).
    pub async fn create_webhook(
        &self,
        project_id: &str,
        config: &WebhookConfig,
    ) -> Result<GitLabWebhook, GitLabError> {
        let url = format!("{}/projects/{}/webhooks", self.base_url, project_id);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
            .json(config)
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::CREATED {
            return Err(Self::map_status(response.status()));
        }

        response
            .json()
            .await
            .map_err(|e| GitLabError::ApiError(e.to_string()))
    }

    /// Update an existing webhook for the given project.
    ///
    /// `project_id` can be the numeric project ID or the URL-encoded
    /// `namespace/project` path (e.g. `"group%2Fproject"`).
    pub async fn update_webhook(
        &self,
        project_id: &str,
        webhook_id: u64,
        config: &WebhookConfig,
    ) -> Result<GitLabWebhook, GitLabError> {
        let url = format!(
            "{}/projects/{}/webhooks/{}",
            self.base_url, project_id, webhook_id
        );

        let response = self
            .client
            .put(&url)
            .headers(self.auth_headers())
            .json(config)
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::OK {
            return Err(Self::map_status(response.status()));
        }

        response
            .json()
            .await
            .map_err(|e| GitLabError::ApiError(e.to_string()))
    }

    /// Delete a webhook by ID for the given project.
    ///
    /// `project_id` can be the numeric project ID or the URL-encoded
    /// `namespace/project` path (e.g. `"group%2Fproject"`).
    pub async fn delete_webhook(
        &self,
        project_id: &str,
        webhook_id: u64,
    ) -> Result<(), GitLabError> {
        let url = format!(
            "{}/projects/{}/webhooks/{}",
            self.base_url, project_id, webhook_id
        );

        let response = self
            .client
            .delete(&url)
            .headers(self.auth_headers())
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::NO_CONTENT
            && response.status() != reqwest::StatusCode::OK
        {
            return Err(Self::map_status(response.status()));
        }

        Ok(())
    }

    // -- Catch-up: Project Events API -----------------------------------------

    /// List project events since a given ISO 8601 timestamp.
    ///
    /// `project_id` can be the numeric project ID or the URL-encoded
    /// `namespace/project` path (e.g. `"group%2Fproject"`).
    /// `after` is an ISO 8601 timestamp (e.g. `"2023-01-01T00:00:00Z"`).
    ///
    /// Handles pagination transparently — iterates until the GitLab API
    /// no longer returns a `rel="next"` Link header.
    pub async fn list_project_events(
        &self,
        project_id: &str,
        after: &str,
    ) -> Result<Vec<ProjectEvent>, GitLabError> {
        let mut all_events = Vec::new();
        let mut next_url: Option<String> = Some(format!(
            "{}/projects/{}/events?after={}",
            self.base_url, project_id, after
        ));

        while let Some(url) = next_url {
            let response = self
                .client
                .get(&url)
                .headers(self.auth_headers())
                .send()
                .await?;

            if response.status() != reqwest::StatusCode::OK {
                return Err(Self::map_status(response.status()));
            }

            next_url = Self::parse_next_link(response.headers());
            let page: Vec<ProjectEvent> = response
                .json()
                .await
                .map_err(|e| GitLabError::ApiError(e.to_string()))?;
            all_events.extend(page);
        }

        Ok(all_events)
    }
}

// ---------------------------------------------------------------------------
// Project event → TriggerEvent mapping (catch-up)
// ---------------------------------------------------------------------------

use crate::webhook::{TriggerEvent, TriggerType};

impl ProjectEvent {
    /// Convert this project event into a Yoke [`TriggerEvent`], or return
    /// `None` if the event type does not map to any known trigger.
    ///
    /// The mapping follows the same logic as the live webhook handler in
    /// [`crate::webhook::gitlab::map_to_trigger_event`]:
    ///
    /// | `action_name`            | `target_type`     | `TriggerType`                           |
    /// |--------------------------|-------------------|-----------------------------------------|
    /// | `"opened"` / `"updated"` | `"Issue"`         | `GitlabIssueAssigned`                   |
    /// | `"commented"`            | `"Issue"`         | `GitlabIssueMention`                    |
    /// | `"commented"`            | `"MergeRequest"`  | `GitlabMergeRequestReview`              |
    /// | other                    | other             | `None` (skipped)                        |
    ///
    /// Push events (`action_name == "pushed_to"`) are currently skipped because
    /// there is no push trigger type yet. This will be added when push
    /// catch-up support lands.
    ///
    /// `repo_path` is the full `namespace/project` path (e.g.
    /// `"internal-team/backend-service"`), injected by the caller since the
    /// Project Events API does not include project path in the response.
    pub fn try_into_trigger_event(self, repo_path: String) -> Option<TriggerEvent> {
        // 1. Determine TriggerType based on action_name and target_type.
        let trigger_type = match (self.action_name.as_str(), self.target_type.as_deref()) {
            ("opened" | "updated", Some("Issue")) => {
                Some(TriggerType::GitlabIssueAssigned { assigned_to: None })
            }
            ("commented", Some("Issue")) => Some(TriggerType::GitlabIssueMention {
                mentioned_user: None,
            }),
            ("commented", Some("MergeRequest")) => Some(TriggerType::GitlabMergeRequestReview),
            _ => None,
        };

        let trigger_type = trigger_type?;

        // 2. Construct canonical event_id (matching Appendix A of the
        //    architecture design).
        let event_id = match (self.action_name.as_str(), self.target_type.as_deref()) {
            (_, Some("Issue")) => format!("issue-{}", self.target_id.unwrap_or(0)),
            (_, Some("MergeRequest")) => {
                if self.action_name == "commented" {
                    format!("mr-{}-comment-{}", self.target_id.unwrap_or(0), self.id)
                } else {
                    format!("mr-{}-review-{}", self.target_id.unwrap_or(0), self.id)
                }
            }
            _ => format!("event-{}", self.id),
        };

        // 3. Extract trigger-specific template variables.
        let mut variables = std::collections::HashMap::new();
        variables.insert("event_id".to_string(), self.id.to_string());
        if let Some(ref push) = self.push_data {
            variables.insert("branch".to_string(), push.branch.clone());
            variables.insert("pushed_sha".to_string(), push.commit_sha.clone());
        }

        Some(TriggerEvent {
            trigger_type,
            repo_path,
            event_id,
            actor: self.author.username,
            variables,
        })
    }
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Find a webhook in a list by its URL (idempotency check).
///
/// Returns the first [`GitLabWebhook`] whose `url` field matches the
/// target, or `None` if no match is found.
pub fn find_webhook_by_url<'a>(
    webhooks: &'a [GitLabWebhook],
    url: &str,
) -> Option<&'a GitLabWebhook> {
    webhooks.iter().find(|w| w.url == url)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    // -- WebhookConfig serialization tests ------------------------------------

    #[test]
    fn test_webhook_config_serialization_full() {
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: Some("secret123".to_string()),
            push_disabled: Some(false),
            active: Some(true),
            events: Some(vec![
                crate::webhook::gitlab::GITLAB_PUSH.to_string(),
                crate::webhook::gitlab::GITLAB_MERGE_REQUESTS.to_string(),
            ]),
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["url"], "https://example.com/hook");
        assert_eq!(json["token"], "secret123");
        assert_eq!(json["push_disabled"], false);
        assert_eq!(json["active"], true);
        let events = json["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], crate::webhook::gitlab::GITLAB_PUSH);
        assert_eq!(events[1], crate::webhook::gitlab::GITLAB_MERGE_REQUESTS);
    }

    #[test]
    fn test_webhook_config_serialization_minimal() {
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: None,
            push_disabled: None,
            active: None,
            events: None,
        };
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["url"], "https://example.com/hook");
        // Fields with skip_serializing_if should be absent when None
        assert!(json.get("token").is_none());
        assert!(json.get("push_disabled").is_none());
        assert!(json.get("active").is_none());
        assert!(json.get("events").is_none());
    }

    // -- create_webhook tests -------------------------------------------------

    #[tokio::test]
    async fn test_create_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"{
            "id": 456,
            "url": "https://example.com/hook",
            "push_disabled": false,
            "active": true
        }"#;

        server
            .mock("POST", "/projects/1/webhooks")
            .match_header("PRIVATE-TOKEN", "test-token")
            .with_status(201)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: Some("secret123".to_string()),
            push_disabled: Some(false),
            active: Some(true),
            events: Some(vec![crate::webhook::gitlab::GITLAB_PUSH.to_string()]),
        };
        let result = client.create_webhook("1", &config).await.unwrap();

        assert_eq!(result.id, 456);
        assert_eq!(result.url, "https://example.com/hook");
        assert!(!result.push_disabled);
        assert!(result.active);
    }

    #[tokio::test]
    async fn test_create_webhook_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("POST", "/projects/1/webhooks")
            .with_status(401)
            .create_async()
            .await;

        let client = GitLabClient::new("bad-token".to_string(), Some(url));
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: None,
            push_disabled: None,
            active: None,
            events: None,
        };
        let result = client.create_webhook("1", &config).await;

        assert!(matches!(result, Err(GitLabError::Unauthorized)));
    }

    // -- update_webhook tests -------------------------------------------------

    #[tokio::test]
    async fn test_update_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"{
            "id": 456,
            "url": "https://example.com/hook",
            "push_disabled": true,
            "active": true
        }"#;

        server
            .mock("PUT", "/projects/1/webhooks/456")
            .match_header("PRIVATE-TOKEN", "test-token")
            .with_status(200)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: Some("new-secret".to_string()),
            push_disabled: Some(true),
            active: Some(true),
            events: Some(vec![
                crate::webhook::gitlab::GITLAB_PUSH.to_string(),
                crate::webhook::gitlab::GITLAB_MERGE_REQUESTS.to_string(),
            ]),
        };
        let result = client.update_webhook("1", 456, &config).await.unwrap();

        assert_eq!(result.id, 456);
        assert_eq!(result.url, "https://example.com/hook");
        assert!(result.push_disabled);
        assert!(result.active);
    }

    #[tokio::test]
    async fn test_update_webhook_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("PUT", "/projects/1/webhooks/999")
            .with_status(404)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: None,
            push_disabled: None,
            active: None,
            events: None,
        };
        let result = client.update_webhook("1", 999, &config).await;

        assert!(matches!(result, Err(GitLabError::NotFound)));
    }

    #[tokio::test]
    async fn test_update_webhook_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("PUT", "/projects/1/webhooks/456")
            .with_status(401)
            .create_async()
            .await;

        let client = GitLabClient::new("bad-token".to_string(), Some(url));
        let config = WebhookConfig {
            url: "https://example.com/hook".to_string(),
            token: None,
            push_disabled: None,
            active: None,
            events: None,
        };
        let result = client.update_webhook("1", 456, &config).await;

        assert!(matches!(result, Err(GitLabError::Unauthorized)));
    }

    // -- delete_webhook tests -------------------------------------------------

    #[tokio::test]
    async fn test_delete_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("DELETE", "/projects/1/webhooks/456")
            .match_header("PRIVATE-TOKEN", "test-token")
            .with_status(204)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.delete_webhook("1", 456).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delete_webhook_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("DELETE", "/projects/1/webhooks/999")
            .with_status(404)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.delete_webhook("1", 999).await;

        assert!(matches!(result, Err(GitLabError::NotFound)));
    }

    #[tokio::test]
    async fn test_delete_webhook_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("DELETE", "/projects/1/webhooks/456")
            .with_status(401)
            .create_async()
            .await;

        let client = GitLabClient::new("bad-token".to_string(), Some(url));
        let result = client.delete_webhook("1", 456).await;

        assert!(matches!(result, Err(GitLabError::Unauthorized)));
    }

    // -- Existing list/find tests (preserved) ----------------------------------

    #[tokio::test]
    async fn test_list_webhooks_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"[
            { "id": 123, "url": "https://example.com/hook", "push_disabled": false, "active": true }
        ]"#;

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(200)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 123);
        assert_eq!(result[0].url, "https://example.com/hook");
        assert!(result[0].active);
        assert!(!result[0].push_disabled);
    }

    #[tokio::test]
    async fn test_list_webhooks_empty() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await.unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_list_webhooks_pagination() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let page1 = r#"[{ "id": 1, "url": "u1", "push_disabled": false, "active": true }]"#;
        let page2 = r#"[{ "id": 2, "url": "u2", "push_disabled": true, "active": false }]"#;

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(200)
            .with_header(
                "link",
                &format!(r#"<{}/projects/1/webhooks?page=2>; rel="next""#, url),
            )
            .with_body(page1)
            .create_async()
            .await;

        server
            .mock("GET", "/projects/1/webhooks?page=2")
            .with_status(200)
            .with_body(page2)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
        assert_eq!(result[1].id, 2);
        assert!(!result[1].active);
        assert!(result[1].push_disabled);
    }

    #[tokio::test]
    async fn test_list_webhooks_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(401)
            .create_async()
            .await;

        let client = GitLabClient::new("bad-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await;

        assert!(matches!(result, Err(GitLabError::Unauthorized)));
    }

    #[tokio::test]
    async fn test_list_webhooks_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(404)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await;

        assert!(matches!(result, Err(GitLabError::NotFound)));
    }

    #[tokio::test]
    async fn test_list_webhooks_api_error() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/1/webhooks")
            .with_status(500)
            .create_async()
            .await;

        let client = GitLabClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("1").await;

        assert!(matches!(result, Err(GitLabError::ApiError(_))));
    }

    #[test]
    fn test_find_webhook_by_url_found() {
        let webhooks = vec![
            GitLabWebhook {
                id: 1,
                url: "https://example.com/hook1".to_string(),
                push_disabled: false,
                active: true,
            },
            GitLabWebhook {
                id: 2,
                url: "https://example.com/hook2".to_string(),
                push_disabled: false,
                active: true,
            },
        ];
        let found = find_webhook_by_url(&webhooks, "https://example.com/hook2").unwrap();
        assert_eq!(found.id, 2);
    }

    #[test]
    fn test_find_webhook_by_url_not_found() {
        let webhooks = vec![GitLabWebhook {
            id: 1,
            url: "https://example.com/hook1".to_string(),
            push_disabled: false,
            active: true,
        }];
        assert!(find_webhook_by_url(&webhooks, "https://example.com/nonexistent").is_none());
    }

    #[test]
    fn test_find_webhook_by_url_empty_list() {
        let webhooks: Vec<GitLabWebhook> = vec![];
        assert!(find_webhook_by_url(&webhooks, "https://example.com/hook1").is_none());
    }

    #[test]
    fn test_client_default_base_url() {
        let client = GitLabClient::new("token".to_string(), None);
        assert_eq!(client.base_url, "https://gitlab.com/api/v4");
    }

    #[test]
    fn test_client_custom_base_url() {
        let client = GitLabClient::new(
            "token".to_string(),
            Some("https://gitlab.example.com/api/v4".to_string()),
        );
        assert_eq!(client.base_url, "https://gitlab.example.com/api/v4");
    }

    // -- list_project_events tests ---------------------------------------------

    #[tokio::test]
    async fn test_list_project_events_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_body = r#"[
            {
                "id": 123,
                "action_name": "opened",
                "created_at": "2023-01-01T10:00:00Z",
                "author": { "username": "testuser" },
                "target_type": "Issue",
                "target_id": 45
            }
        ]"#;

        server
            .mock("GET", "/projects/1/events")
            .match_query(mockito::Matcher::UrlEncoded(
                "after".to_string(),
                "2023-01-01T00:00:00Z".to_string(),
            ))
            .with_status(200)
            .with_body(mock_body)
            .create_async()
            .await;

        let client = GitLabClient::new("token".to_string(), Some(url));
        let events = client
            .list_project_events("1", "2023-01-01T00:00:00Z")
            .await
            .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, 123);
        assert_eq!(events[0].action_name, "opened");
        assert_eq!(events[0].target_type.as_deref(), Some("Issue"));
        assert_eq!(events[0].target_id, Some(45));
    }

    #[tokio::test]
    async fn test_list_project_events_pagination() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let page1 = r#"[
            {
                "id": 1,
                "action_name": "opened",
                "created_at": "2023-01-01T10:00:00Z",
                "author": { "username": "alice" },
                "target_type": "Issue",
                "target_id": 10
            }
        ]"#;
        let page2 = r#"[
            {
                "id": 2,
                "action_name": "commented",
                "created_at": "2023-01-01T11:00:00Z",
                "author": { "username": "bob" },
                "target_type": "MergeRequest",
                "target_id": 20
            }
        ]"#;

        server
            .mock("GET", "/projects/1/events")
            .match_query(mockito::Matcher::UrlEncoded(
                "after".to_string(),
                "2023-01-01T00:00:00Z".to_string(),
            ))
            .with_status(200)
            .with_header(
                "link",
                &format!(
                    r#"<{}/projects/1/events?after=2023-01-01T00:00:00Z&page=2>; rel="next""#,
                    url
                ),
            )
            .with_body(page1)
            .create_async()
            .await;

        server
            .mock("GET", "/projects/1/events")
            .match_query(mockito::Matcher::AllOf(vec![
                mockito::Matcher::UrlEncoded(
                    "after".to_string(),
                    "2023-01-01T00:00:00Z".to_string(),
                ),
                mockito::Matcher::UrlEncoded("page".to_string(), "2".to_string()),
            ]))
            .with_status(200)
            .with_body(page2)
            .create_async()
            .await;

        let client = GitLabClient::new("token".to_string(), Some(url));
        let events = client
            .list_project_events("1", "2023-01-01T00:00:00Z")
            .await
            .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, 1);
        assert_eq!(events[1].id, 2);
    }

    #[tokio::test]
    async fn test_list_project_events_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/1/events")
            .match_query(mockito::Matcher::UrlEncoded(
                "after".to_string(),
                "2023-01-01T00:00:00Z".to_string(),
            ))
            .with_status(401)
            .create_async()
            .await;

        let client = GitLabClient::new("bad-token".to_string(), Some(url));
        let result = client
            .list_project_events("1", "2023-01-01T00:00:00Z")
            .await;

        assert!(matches!(result, Err(GitLabError::Unauthorized)));
    }

    #[tokio::test]
    async fn test_list_project_events_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/projects/999/events")
            .match_query(mockito::Matcher::UrlEncoded(
                "after".to_string(),
                "2023-01-01T00:00:00Z".to_string(),
            ))
            .with_status(404)
            .create_async()
            .await;

        let client = GitLabClient::new("token".to_string(), Some(url));
        let result = client
            .list_project_events("999", "2023-01-01T00:00:00Z")
            .await;

        assert!(matches!(result, Err(GitLabError::NotFound)));
    }

    // -- ProjectEvent → TriggerEvent mapping tests -----------------------------

    #[test]
    fn test_project_event_mapping_issue_opened() {
        let event = ProjectEvent {
            id: 100,
            action_name: "opened".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "alice".to_string(),
            },
            push_data: None,
            target_type: Some("Issue".to_string()),
            target_id: Some(42),
        };

        let trigger = event
            .try_into_trigger_event("org/repo".to_string())
            .unwrap();
        assert_eq!(trigger.event_id, "issue-42");
        assert_eq!(trigger.actor, "alice");
        assert_eq!(trigger.repo_path, "org/repo");
        assert!(matches!(
            trigger.trigger_type,
            crate::workflow::TriggerType::GitlabIssueAssigned { assigned_to: None }
        ));
    }

    #[test]
    fn test_project_event_mapping_issue_updated() {
        let event = ProjectEvent {
            id: 101,
            action_name: "updated".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "bob".to_string(),
            },
            push_data: None,
            target_type: Some("Issue".to_string()),
            target_id: Some(7),
        };

        let trigger = event
            .try_into_trigger_event("org/repo".to_string())
            .unwrap();
        assert_eq!(trigger.event_id, "issue-7");
        assert!(matches!(
            trigger.trigger_type,
            crate::workflow::TriggerType::GitlabIssueAssigned { assigned_to: None }
        ));
    }

    #[test]
    fn test_project_event_mapping_issue_comment() {
        let event = ProjectEvent {
            id: 200,
            action_name: "commented".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "charlie".to_string(),
            },
            push_data: None,
            target_type: Some("Issue".to_string()),
            target_id: Some(7),
        };

        let trigger = event
            .try_into_trigger_event("org/repo".to_string())
            .unwrap();
        assert_eq!(trigger.event_id, "issue-7");
        assert!(matches!(
            trigger.trigger_type,
            crate::workflow::TriggerType::GitlabIssueMention {
                mentioned_user: None
            }
        ));
    }

    #[test]
    fn test_project_event_mapping_mr_comment() {
        let event = ProjectEvent {
            id: 300,
            action_name: "commented".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "dave".to_string(),
            },
            push_data: None,
            target_type: Some("MergeRequest".to_string()),
            target_id: Some(5),
        };

        let trigger = event
            .try_into_trigger_event("org/repo".to_string())
            .unwrap();
        // Commented on MR → mr-{iid}-comment-{id}
        assert_eq!(trigger.event_id, "mr-5-comment-300");
        assert!(matches!(
            trigger.trigger_type,
            crate::workflow::TriggerType::GitlabMergeRequestReview
        ));
    }

    #[test]
    fn test_project_event_mapping_mr_review() {
        let event = ProjectEvent {
            id: 301,
            action_name: "approved".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "eve".to_string(),
            },
            push_data: None,
            target_type: Some("MergeRequest".to_string()),
            target_id: Some(5),
        };

        // Non-commented action on MR → no mapping currently
        let result = event.try_into_trigger_event("org/repo".to_string());
        assert!(result.is_none());
    }

    #[test]
    fn test_project_event_mapping_unsupported_action_returns_none() {
        let event = ProjectEvent {
            id: 400,
            action_name: "pushed_to".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "frank".to_string(),
            },
            push_data: Some(ProjectPushData {
                commit_sha: "abc123".to_string(),
                branch: "main".to_string(),
                commit_count: 1,
            }),
            target_type: None,
            target_id: None,
        };

        // Push events don't map to any trigger type yet
        let result = event.try_into_trigger_event("org/repo".to_string());
        assert!(result.is_none());
    }

    #[test]
    fn test_project_event_mapping_includes_variables() {
        let event = ProjectEvent {
            id: 500,
            action_name: "commented".to_string(),
            created_at: chrono::Utc::now(),
            author: crate::webhook::gitlab::GitLabUser {
                username: "alice".to_string(),
            },
            push_data: Some(ProjectPushData {
                commit_sha: "deadbeef".to_string(),
                branch: "feature".to_string(),
                commit_count: 3,
            }),
            target_type: Some("Issue".to_string()),
            target_id: Some(10),
        };

        let trigger = event
            .try_into_trigger_event("org/repo".to_string())
            .unwrap();
        assert_eq!(trigger.variables.get("event_id").unwrap(), "500");
        assert_eq!(trigger.variables.get("branch").unwrap(), "feature");
        assert_eq!(trigger.variables.get("pushed_sha").unwrap(), "deadbeef");
    }
}
