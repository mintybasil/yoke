use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors returned by the GitHub REST API client.
#[derive(Error, Debug)]
pub enum GitHubError {
    /// The HTTP request itself failed (network error, timeout, etc.).
    #[error("HTTP request failed: {0}")]
    RequestError(#[from] reqwest::Error),
    /// Authentication failed (HTTP 401).
    #[error("Authentication failed (401)")]
    Unauthorized,
    /// The requested resource was not found (HTTP 404).
    #[error("Resource not found (404)")]
    NotFound,
    /// GitHub rate limit exceeded (HTTP 403).
    #[error("Rate limit exceeded")]
    RateLimited,
    /// Invalid webhook configuration (e.g., empty events list).
    #[error("Invalid webhook configuration: {0}")]
    ValidationError(String),
    /// Any other API error with an HTTP status code and response body.
    #[error("API error: {0}")]
    ApiError(String),
}

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

/// A GitHub repository webhook.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Webhook {
    pub id: u64,
    pub url: String,
    pub secret: Option<String>,
    pub events: Vec<String>,
    pub active: bool,
}

/// Nested configuration passed inside the `config` key of the GitHub webhook API.
///
/// GitHub's API requires `url` and `secret` inside a `config` object,
/// not at the top level. See: <https://docs.github.com/rest/repos/webhooks#create-a-repository-webhook>
#[derive(Debug, Clone, Serialize)]
pub struct WebhookConfigInner {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    pub content_type: String,
}

/// Configuration for creating or updating a GitHub webhook.
///
/// Serializes to the JSON shape GitHub expects:
/// `{"config":{"url":"...","secret":"...","content_type":"json"},"events":[...],"active":true}`
#[derive(Debug, Clone, Serialize)]
pub struct WebhookConfig {
    pub config: WebhookConfigInner,
    pub events: Vec<String>,
    pub active: bool,
}

/// Summary of a webhook orchestration operation across one or more repositories.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq)]
pub struct WebhookOrchestrationSummary {
    pub created: u32,
    pub updated: u32,
    pub skipped: u32,
}

impl WebhookOrchestrationSummary {
    pub fn add_created(&mut self) {
        self.created += 1;
    }
    pub fn add_updated(&mut self) {
        self.updated += 1;
    }
    pub fn add_skipped(&mut self) {
        self.skipped += 1;
    }
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// REST API client for GitHub.
///
/// Wraps a [`reqwest::Client`] and handles authentication headers,
/// error mapping, and pagination transparently.
#[derive(Debug, Clone)]
pub struct GitHubClient {
    token: String,
    base_url: String,
    client: Client,
}

impl GitHubClient {
    /// Create a new client.
    ///
    /// If `base_url` is `None`, defaults to `https://api.github.com`.
    pub fn new(token: String, base_url: Option<String>) -> Self {
        Self {
            token,
            base_url: base_url.unwrap_or_else(|| "https://api.github.com".to_string()),
            client: Client::new(),
        }
    }

    // -- Internal helpers ----------------------------------------------------

    /// Find a webhook in a list by its URL. Used for idempotency checks.
    fn find_webhook_by_url<'a>(&self, webhooks: &'a [Webhook], url: &str) -> Option<&'a Webhook> {
        webhooks.iter().find(|w| w.url == url)
    }

    /// Build the authentication + user-agent headers that every request needs.
    fn auth_headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", self.token)
                .parse()
                .expect("header value should be valid"),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            "yoke-agent".parse().expect("header value should be valid"),
        );
        headers
    }

    /// Map an HTTP status code and response body to a [`GitHubError`].
    ///
    /// Includes the response body in the error message for non-specific status
    /// codes, making debugging API errors easier. Well-known status codes (401,
    /// 404, 403) are still mapped to their dedicated error variants.
    fn map_status_with_body(status: reqwest::StatusCode, body: &str) -> GitHubError {
        match status {
            reqwest::StatusCode::UNAUTHORIZED => GitHubError::Unauthorized,
            reqwest::StatusCode::NOT_FOUND => GitHubError::NotFound,
            reqwest::StatusCode::FORBIDDEN => GitHubError::RateLimited,
            other => GitHubError::ApiError(format!("{} - {}", other, body)),
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

    /// List all webhooks for the given repository.
    ///
    /// Handles pagination transparently — iterates until the GitHub API
    /// no longer returns a `rel="next"` Link header.
    pub async fn list_webhooks(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<Webhook>, GitHubError> {
        let mut all_webhooks = Vec::new();
        let mut next_url: Option<String> =
            Some(format!("{}/repos/{}/{}/hooks", self.base_url, owner, repo));

        while let Some(url) = next_url {
            let response = self
                .client
                .get(&url)
                .headers(self.auth_headers())
                .send()
                .await?;

            if response.status() != reqwest::StatusCode::OK {
                let status = response.status();
                let body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "could not read body".to_string());
                return Err(Self::map_status_with_body(status, &body));
            }

            // Capture pagination before consuming the body.
            next_url = Self::parse_next_link(response.headers());

            let page: Vec<Webhook> = response.json().await?;
            all_webhooks.extend(page);
        }

        Ok(all_webhooks)
    }

    /// Create a new webhook for the given repository.
    pub async fn create_webhook(
        &self,
        owner: &str,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<Webhook, GitHubError> {
        if config.events.is_empty() {
            return Err(GitHubError::ValidationError(
                "at least one event must be specified".to_string(),
            ));
        }

        let url = format!("{}/repos/{}/{}/hooks", self.base_url, owner, repo);

        let response = self
            .client
            .post(&url)
            .headers(self.auth_headers())
            .json(config)
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::OK
            && response.status() != reqwest::StatusCode::CREATED
        {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "could not read body".to_string());
            return Err(Self::map_status_with_body(status, &body));
        }

        response.json().await.map_err(GitHubError::RequestError)
    }

    /// Update an existing webhook for the given repository.
    pub async fn update_webhook(
        &self,
        owner: &str,
        repo: &str,
        webhook_id: u64,
        config: &WebhookConfig,
    ) -> Result<Webhook, GitHubError> {
        if config.events.is_empty() {
            return Err(GitHubError::ValidationError(
                "at least one event must be specified".to_string(),
            ));
        }

        let url = format!(
            "{}/repos/{}/{}/hooks/{}",
            self.base_url, owner, repo, webhook_id
        );

        let response = self
            .client
            .patch(&url)
            .headers(self.auth_headers())
            .json(config)
            .send()
            .await?;

        if response.status() != reqwest::StatusCode::OK {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "could not read body".to_string());
            return Err(Self::map_status_with_body(status, &body));
        }

        response.json().await.map_err(GitHubError::RequestError)
    }

    /// Delete a webhook for the given repository.
    pub async fn delete_webhook(
        &self,
        owner: &str,
        repo: &str,
        webhook_id: u64,
    ) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/hooks/{}",
            self.base_url, owner, repo, webhook_id
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
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "could not read body".to_string());
            return Err(Self::map_status_with_body(status, &body));
        }

        Ok(())
    }

    /// Ensures a webhook exists with the given configuration.
    ///
    /// If a webhook with the same URL already exists, it updates it.
    /// Otherwise, it creates a new one.
    pub async fn ensure_webhook(
        &self,
        owner: &str,
        repo: &str,
        config: &WebhookConfig,
    ) -> Result<WebhookOrchestrationSummary, GitHubError> {
        let mut summary = WebhookOrchestrationSummary::default();
        let webhooks = self.list_webhooks(owner, repo).await?;

        if let Some(existing) = self.find_webhook_by_url(&webhooks, &config.config.url) {
            self.update_webhook(owner, repo, existing.id, config)
                .await?;
            summary.add_updated();
        } else {
            self.create_webhook(owner, repo, config).await?;
            summary.add_created();
        }

        Ok(summary)
    }

    /// Idempotently ensures webhooks are configured across a list of repositories.
    ///
    /// For each repository, checks if a webhook with the configured URL already
    /// exists. If so, updates it; otherwise, creates a new one. Aggregates results
    /// across all repositories into a single summary.
    pub async fn orchestrate_webhooks(
        &self,
        repos: Vec<(String, String)>,
        config: &WebhookConfig,
    ) -> Result<WebhookOrchestrationSummary, GitHubError> {
        let mut total_summary = WebhookOrchestrationSummary::default();

        for (owner, repo) in repos {
            let repo_summary = self.ensure_webhook(&owner, &repo, config).await?;
            total_summary.created += repo_summary.created;
            total_summary.updated += repo_summary.updated;
            total_summary.skipped += repo_summary.skipped;
        }

        Ok(total_summary)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::Server;

    #[tokio::test]
    async fn test_list_webhooks_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"[
            { "id": 123, "url": "https://api.github.com/repos/owner/repo/hooks/123", "secret": "s3cret", "events": ["push"], "active": true }
        ]"#;

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(200)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await.unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, 123);
        assert_eq!(result[0].events, vec!["push"]);
        assert!(result[0].active);
    }

    #[tokio::test]
    async fn test_list_webhooks_empty() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await.unwrap();

        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_list_webhooks_pagination() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let page1 = r#"[{ "id": 1, "url": "u1", "secret": null, "events": [], "active": true }]"#;
        let page2 =
            r#"[{ "id": 2, "url": "u2", "secret": null, "events": ["push"], "active": false }]"#;

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(200)
            .with_header(
                "link",
                &format!(r#"<{}/repos/owner/repo/hooks?page=2>; rel="next""#, url),
            )
            .with_body(page1)
            .create_async()
            .await;

        server
            .mock("GET", "/repos/owner/repo/hooks?page=2")
            .with_status(200)
            .with_body(page2)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await.unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
        assert_eq!(result[1].id, 2);
        assert!(!result[1].active);
    }

    #[tokio::test]
    async fn test_list_webhooks_unauthorized() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(401)
            .create_async()
            .await;

        let client = GitHubClient::new("bad-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await;

        assert!(matches!(result, Err(GitHubError::Unauthorized)));
    }

    #[tokio::test]
    async fn test_list_webhooks_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(404)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await;

        assert!(matches!(result, Err(GitHubError::NotFound)));
    }

    #[tokio::test]
    async fn test_list_webhooks_rate_limited() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(403)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await;

        assert!(matches!(result, Err(GitHubError::RateLimited)));
    }

    #[tokio::test]
    async fn test_create_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"{ "id": 456, "url": "https://api.github.com/repos/owner/repo/hooks/456", "secret": null, "events": ["push"], "active": true }"#;

        server
            .mock("POST", "/repos/owner/repo/hooks")
            .with_status(201)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("secret123".to_string()),
                content_type: "json".to_string(),
            },
            events: vec!["push".to_string()],
            active: true,
        };
        let result = client
            .create_webhook("owner", "repo", &config)
            .await
            .unwrap();

        assert_eq!(result.id, 456);
    }

    #[tokio::test]
    async fn test_update_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let mock_response = r#"{ "id": 456, "url": "https://api.github.com/repos/owner/repo/hooks/456", "secret": null, "events": ["push", "pull_request"], "active": true }"#;

        server
            .mock("PATCH", "/repos/owner/repo/hooks/456")
            .with_status(200)
            .with_body(mock_response)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("new-secret".to_string()),
                content_type: "json".to_string(),
            },
            events: vec!["push".to_string(), "pull_request".to_string()],
            active: true,
        };
        let result = client
            .update_webhook("owner", "repo", 456, &config)
            .await
            .unwrap();

        assert_eq!(result.id, 456);
        assert_eq!(result.events, vec!["push", "pull_request"]);
    }

    #[tokio::test]
    async fn test_create_webhook_empty_events_validation() {
        let client = GitHubClient::new("test-token".to_string(), None);
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("s".to_string()),
                content_type: "json".to_string(),
            },
            events: vec![],
            active: true,
        };
        let result = client.create_webhook("owner", "repo", &config).await;

        assert!(
            matches!(result, Err(GitHubError::ValidationError(msg)) if msg.contains("at least one event"))
        );
    }

    #[tokio::test]
    async fn test_create_webhook_422_with_body() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let error_body = r#"{"message":"Validation Failed","errors":[{"resource":"Hook","code":"custom","message":"Hook already exists on this repository"}]}"#;

        server
            .mock("POST", "/repos/owner/repo/hooks")
            .with_status(422)
            .with_header("content-type", "application/json")
            .with_body(error_body)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("s".to_string()),
                content_type: "json".to_string(),
            },
            events: vec!["push".to_string()],
            active: true,
        };
        let result = client.create_webhook("owner", "repo", &config).await;

        assert!(
            matches!(result, Err(GitHubError::ApiError(msg)) if msg.contains("422") && msg.contains("Validation Failed"))
        );
    }

    #[tokio::test]
    async fn test_update_webhook_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("PATCH", "/repos/owner/repo/hooks/999")
            .with_status(404)
            .with_body("{\"message\":\"Not Found\"}")
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("s".to_string()),
                content_type: "json".to_string(),
            },
            events: vec!["push".to_string()],
            active: true,
        };
        let result = client.update_webhook("owner", "repo", 999, &config).await;

        assert!(matches!(result, Err(GitHubError::NotFound)));
    }

    #[tokio::test]
    async fn test_update_webhook_empty_events_validation() {
        let client = GitHubClient::new("test-token".to_string(), None);
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "http://example.com/webhook".to_string(),
                secret: Some("s".to_string()),
                content_type: "json".to_string(),
            },
            events: vec![],
            active: true,
        };
        let result = client.update_webhook("owner", "repo", 999, &config).await;

        assert!(
            matches!(result, Err(GitHubError::ValidationError(msg)) if msg.contains("at least one event"))
        );
    }

    #[test]
    fn test_find_webhook_by_url() {
        let client = GitHubClient::new("t".to_string(), None);
        let webhooks = vec![
            Webhook {
                id: 1,
                url: "u1".to_string(),
                secret: None,
                events: vec![],
                active: true,
            },
            Webhook {
                id: 2,
                url: "u2".to_string(),
                secret: None,
                events: vec![],
                active: true,
            },
        ];

        assert_eq!(client.find_webhook_by_url(&webhooks, "u2").unwrap().id, 2);
        assert!(client.find_webhook_by_url(&webhooks, "u3").is_none());
    }

    // -- delete_webhook tests ------------------------------------------------

    #[tokio::test]
    async fn test_delete_webhook_success() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("DELETE", "/repos/owner/repo/hooks/123")
            .with_status(204)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.delete_webhook("owner", "repo", 123).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_delete_webhook_not_found() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("DELETE", "/repos/owner/repo/hooks/999")
            .with_status(404)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.delete_webhook("owner", "repo", 999).await;

        assert!(matches!(result, Err(GitHubError::NotFound)));
    }

    #[tokio::test]
    async fn test_delete_webhook_422_with_body() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let error_body = r#"{"message":"Validation Failed","errors":[{"resource":"Hook","code":"custom","message":"Hook is in an invalid state"}]}"#;

        server
            .mock("DELETE", "/repos/owner/repo/hooks/123")
            .with_status(422)
            .with_header("content-type", "application/json")
            .with_body(error_body)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.delete_webhook("owner", "repo", 123).await;

        assert!(
            matches!(result, Err(GitHubError::ApiError(msg)) if msg.contains("422") && msg.contains("Validation Failed"))
        );
    }

    #[tokio::test]
    async fn test_list_webhooks_422_with_body() {
        let mut server = Server::new_async().await;
        let url = server.url();

        let error_body = r#"{"message":"Validation Failed","errors":[{"resource":"Hook","code":"custom","message":"Invalid request"}]}"#;

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(422)
            .with_header("content-type", "application/json")
            .with_body(error_body)
            .create_async()
            .await;

        let client = GitHubClient::new("test-token".to_string(), Some(url));
        let result = client.list_webhooks("owner", "repo").await;

        assert!(
            matches!(result, Err(GitHubError::ApiError(msg)) if msg.contains("422") && msg.contains("Validation Failed"))
        );
    }

    // -- ensure_webhook tests -------------------------------------------------

    #[tokio::test]
    async fn test_ensure_webhook_creates_new() {
        let mut server = Server::new_async().await;
        let url = server.url();

        // 1. List returns empty
        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;

        // 2. Create is called
        server
            .mock("POST", "/repos/owner/repo/hooks")
            .with_status(201)
            .with_body(
                r#"{ "id": 123, "url": "u1", "secret": null, "events": [], "active": true }"#,
            )
            .create_async()
            .await;

        let client = GitHubClient::new("token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "u1".into(),
                secret: Some("s".into()),
                content_type: "json".into(),
            },
            events: vec!["push".into()],
            active: true,
        };

        let summary = client
            .ensure_webhook("owner", "repo", &config)
            .await
            .unwrap();
        assert_eq!(summary.created, 1);
        assert_eq!(summary.updated, 0);
    }

    #[tokio::test]
    async fn test_ensure_webhook_updates_existing() {
        let mut server = Server::new_async().await;
        let url = server.url();

        server
            .mock("GET", "/repos/owner/repo/hooks")
            .with_status(200)
            .with_body(
                r#"[{ "id": 123, "url": "u1", "secret": null, "events": [], "active": true }]"#,
            )
            .create_async()
            .await;

        server
            .mock("PATCH", "/repos/owner/repo/hooks/123")
            .with_status(200)
            .with_body(
                r#"{ "id": 123, "url": "u1", "secret": null, "events": ["push"], "active": true }"#,
            )
            .create_async()
            .await;

        let client = GitHubClient::new("token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "u1".into(),
                secret: Some("s".into()),
                content_type: "json".into(),
            },
            events: vec!["push".into()],
            active: true,
        };

        let summary = client
            .ensure_webhook("owner", "repo", &config)
            .await
            .unwrap();
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.created, 0);
    }

    // -- orchestrate_webhooks tests -------------------------------------------

    #[tokio::test]
    async fn test_orchestrate_webhooks_multi() {
        let mut server = Server::new_async().await;
        let url = server.url();

        // Repo A: Existing (Update)
        server
            .mock("GET", "/repos/owner/repoA/hooks")
            .with_status(200)
            .with_body(r#"[{"id":1, "url":"u1", "secret":null, "events":[], "active":true}]"#)
            .create_async()
            .await;
        server
            .mock("PATCH", "/repos/owner/repoA/hooks/1")
            .with_status(200)
            .with_body(r#"{ "id": 1, "url": "u1", "secret": null, "events": [], "active": true }"#)
            .create_async()
            .await;

        // Repo B: Missing (Create)
        server
            .mock("GET", "/repos/owner/repoB/hooks")
            .with_status(200)
            .with_body("[]")
            .create_async()
            .await;
        server
            .mock("POST", "/repos/owner/repoB/hooks")
            .with_status(201)
            .with_body(r#"{ "id": 2, "url": "u1", "secret": null, "events": [], "active": true }"#)
            .create_async()
            .await;

        let client = GitHubClient::new("token".to_string(), Some(url));
        let config = WebhookConfig {
            config: WebhookConfigInner {
                url: "u1".into(),
                secret: Some("s".into()),
                content_type: "json".into(),
            },
            events: vec!["push".into()],
            active: true,
        };
        let repos = vec![
            ("owner".into(), "repoA".into()),
            ("owner".into(), "repoB".into()),
        ];

        let summary = client.orchestrate_webhooks(repos, &config).await.unwrap();
        assert_eq!(summary.created, 1);
        assert_eq!(summary.updated, 1);
    }
}
