//! Integration tests for webhooks command handlers (add, remove, list).
//!
//! These tests use mockito to mock the GitHub API responses and
//! verify that the high-level handler functions behave correctly.

use std::fs;
use yoke::config::{Config, Repo, ServerConfig};
use yoke::webhooks::{self, AddSummary, GitHubWebhookClient, RemoveSummary, WebhookClient};

/// Helper to create a test config with a single repo.
fn test_config() -> Config {
    Config {
        platform: yoke::config::Platform::Github,
        repos: vec![Repo {
            owner: "test-owner".to_string(),
            repo: "test-repo".to_string(),
        }],
        agents: vec![yoke::config::AgentConfig {
            name: "swe".to_string(),
            base_url: url::Url::parse("http://localhost:8000").unwrap(),
        }],
        runtime: yoke::config::RuntimeConfig::default(),
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            webhook_host: "yoke.example.com".to_string(),
            port: 8644,
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1_048_576,
        },
        github: None,
        gitlab: None,
        gitlab_url: None,
    }
}

/// Create a temporary workflows directory with a valid workflow TOML.
fn create_workflows_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let toml = r#"
[trigger]
type = "github_issue_assigned"

[[steps]]
name = "Plan"
agent = "swe"
prompt_template = "Plan the issue"
"#;
    fs::write(dir.path().join("plan.toml"), toml).unwrap();
    dir
}

/// Build a WebhookClient that talks to the mockito server instead of real GitHub.
fn mock_github_client(mock_url: &str) -> WebhookClient {
    let gh_client =
        GitHubWebhookClient::new_with_base_url("test-token".to_string(), mock_url.to_string());
    WebhookClient::Github(gh_client)
}

// GitHub API uses /repos/{owner}/{repo}/hooks for all webhook operations.

#[tokio::test]
async fn test_webhooks_list_empty() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_list(&config, &client).await;
    assert!(result.is_ok());
    mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_list_with_hooks() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let body = r#"{"id":1,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/1","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["issues"],"active":true}"#;
    let mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{body}]"))
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_list(&config, &client).await;
    assert!(result.is_ok());
    mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_remove_no_matching() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let body = r#"[{"id":1,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/1","config":{"url":"https://other.example.com/hook","content_type":"json","secret":"***"},"events":["push"],"active":true}]"#;
    let mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_remove(&config, &client).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        RemoveSummary {
            deleted: 0,
            not_found: 1,
            errors: 0
        }
    );
    mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_remove_matching() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let body = r#"[{"id":42,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/42","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["issues"],"active":true}]"#;
    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(body)
        .create_async()
        .await;

    let delete_mock = server
        .mock("DELETE", "/repos/test-owner/test-repo/hooks/42")
        .with_status(204)
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_remove(&config, &client).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        RemoveSummary {
            deleted: 1,
            not_found: 0,
            errors: 0
        }
    );
    list_mock.assert_async().await;
    delete_mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_add_creates_new() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let list_body = r#"[{"id":1,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/1","config":{"url":"https://other.example.com/hook","content_type":"json","secret":"***"},"events":["push"],"active":true}]"#;
    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(list_body)
        .create_async()
        .await;

    let create_body = r#"{"id":99,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/99","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["issues"],"active":true}"#;
    let create_mock = server
        .mock("POST", "/repos/test-owner/test-repo/hooks")
        .with_status(201)
        .with_header("content-type", "application/json")
        .with_body(create_body)
        .create_async()
        .await;

    let config = test_config();
    let workflows_dir = create_workflows_dir();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_add(&config, &client, workflows_dir.path()).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        AddSummary {
            created: 1,
            updated: 0,
            skipped: 0,
            errors: 0
        }
    );
    list_mock.assert_async().await;
    create_mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_add_updates_existing() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let list_body = r#"[{"id":42,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/42","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["push"],"active":true}]"#;
    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(list_body)
        .create_async()
        .await;

    let update_body = r#"{"id":42,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/42","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["issues"],"active":true}"#;
    let update_mock = server
        .mock("PATCH", "/repos/test-owner/test-repo/hooks/42")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(update_body)
        .create_async()
        .await;

    let config = test_config();
    let workflows_dir = create_workflows_dir();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_add(&config, &client, workflows_dir.path()).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        AddSummary {
            created: 0,
            updated: 1,
            skipped: 0,
            errors: 0
        }
    );
    list_mock.assert_async().await;
    update_mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_add_empty_repos() {
    let config = Config {
        platform: yoke::config::Platform::Github,
        repos: vec![],
        agents: vec![yoke::config::AgentConfig {
            name: "swe".to_string(),
            base_url: url::Url::parse("http://localhost:8000").unwrap(),
        }],
        runtime: yoke::config::RuntimeConfig::default(),
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            webhook_host: "yoke.example.com".to_string(),
            port: 8644,
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1_048_576,
        },
        github: None,
        gitlab: None,
        gitlab_url: None,
    };
    let workflows_dir = create_workflows_dir();
    let server = mockito::Server::new_async().await;
    let client = mock_github_client(&server.url());

    let result = webhooks::webhooks_add(&config, &client, workflows_dir.path()).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        AddSummary {
            created: 0,
            updated: 0,
            skipped: 0,
            errors: 0
        }
    );
}

#[tokio::test]
async fn test_webhooks_remove_empty_repos() {
    let config = Config {
        platform: yoke::config::Platform::Github,
        repos: vec![],
        agents: vec![yoke::config::AgentConfig {
            name: "swe".to_string(),
            base_url: url::Url::parse("http://localhost:8000").unwrap(),
        }],
        runtime: yoke::config::RuntimeConfig::default(),
        server: ServerConfig {
            host: "0.0.0.0".to_string(),
            webhook_host: "yoke.example.com".to_string(),
            port: 8644,
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1_048_576,
        },
        github: None,
        gitlab: None,
        gitlab_url: None,
    };
    let server = mockito::Server::new_async().await;
    let client = mock_github_client(&server.url());

    let result = webhooks::webhooks_remove(&config, &client).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        RemoveSummary {
            deleted: 0,
            not_found: 0,
            errors: 0
        }
    );
}

#[tokio::test]
async fn test_webhooks_add_api_error_on_list() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(401)
        .create_async()
        .await;

    let config = test_config();
    let workflows_dir = create_workflows_dir();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_add(&config, &client, workflows_dir.path()).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        AddSummary {
            created: 0,
            updated: 0,
            skipped: 0,
            errors: 1
        }
    );
    list_mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_remove_api_error() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(500)
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_remove(&config, &client).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        RemoveSummary {
            deleted: 0,
            not_found: 0,
            errors: 1
        }
    );
    list_mock.assert_async().await;
}

#[tokio::test]
async fn test_webhooks_remove_delete_error() {
    let mut server = mockito::Server::new_async().await;
    let url = server.url();

    let list_body = r#"[{"id":42,"url":"https://api.github.com/repos/test-owner/test-repo/hooks/42","config":{"url":"https://yoke.example.com/webhook","content_type":"json","secret":"***"},"events":["issues"],"active":true}]"#;
    let list_mock = server
        .mock("GET", "/repos/test-owner/test-repo/hooks")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(list_body)
        .create_async()
        .await;

    let delete_mock = server
        .mock("DELETE", "/repos/test-owner/test-repo/hooks/42")
        .with_status(500)
        .create_async()
        .await;

    let config = test_config();
    let client = mock_github_client(&url);

    let result = webhooks::webhooks_remove(&config, &client).await;
    assert!(result.is_ok());
    let summary = result.unwrap();
    assert_eq!(
        summary,
        RemoveSummary {
            deleted: 0,
            not_found: 0,
            errors: 1
        }
    );
    list_mock.assert_async().await;
    delete_mock.assert_async().await;
}
