use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::config::{AgentConfig, Platform, ServerConfig};
use crate::dispatcher::{Dispatcher, new_dedup_sets};
use crate::reload::WorkflowState;
use crate::webhook;
use tracing::instrument;

/// HTTP header names for webhook authentication and event identification.
pub mod headers {
    /// GitHub HMAC-SHA256 signature header.
    pub const GITHUB_SIGNATURE: &str = "X-Hub-Signature-256";
    /// GitHub event type header.
    pub const GITHUB_EVENT: &str = "X-GitHub-Event";
    /// GitLab webhook token header.
    pub const GITLAB_TOKEN: &str = "X-Gitlab-Token";
    /// GitLab event type header.
    pub const GITLAB_EVENT: &str = "X-Gitlab-Event";
}

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub webhook_handler: webhook::WebhookHandler,
    pub dispatcher: Dispatcher,
}

/// Health check handler — returns `{"status": "ok"}`.
async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Readiness check handler — returns 200 when the server is accepting events,
/// 503 Service Unavailable when the dispatcher is shutting down.
async fn ready(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    if state.dispatcher.is_shutting_down() {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    }
}

/// Webhook handler — routes verified platform events through the `WebhookHandler`
/// and returns appropriate HTTP status codes.
///
/// Extracts authentication and event-type headers, then delegates to
/// `state.webhook_handler.handle_webhook` which verifies, parses, and
/// sends the resulting `TriggerEvent` to the dispatcher channel.
///
/// Returns:
/// - `200 OK` if the event was processed or no matching trigger was found (no-op)
/// - `401 Unauthorized` if signature/token verification fails
/// - `400 Bad Request` if the payload cannot be parsed
/// - `503 Service Unavailable` if the dispatcher channel is full
#[instrument(skip_all, fields(platform = ?state.webhook_handler.platform))]
async fn webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    // Extract authentication header based on platform
    let (token_header, event_header) = match state.webhook_handler.platform {
        Platform::Github => {
            // GitHub uses X-Hub-Signature-256 for HMAC and X-GitHub-Event for type
            let sig = match headers.get(headers::GITHUB_SIGNATURE) {
                Some(v) => match v.to_str() {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        tracing::warn!("invalid GitHub signature header encoding");
                        return StatusCode::UNAUTHORIZED;
                    }
                },
                None => {
                    tracing::warn!("missing GitHub signature header");
                    return StatusCode::UNAUTHORIZED;
                }
            };
            let evt = match headers.get(headers::GITHUB_EVENT) {
                Some(v) => match v.to_str() {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        tracing::warn!("invalid GitHub event header encoding");
                        return StatusCode::BAD_REQUEST;
                    }
                },
                None => {
                    tracing::warn!("missing GitHub event header");
                    return StatusCode::BAD_REQUEST;
                }
            };
            (sig, evt)
        }
        Platform::Gitlab => {
            // GitLab uses X-Gitlab-Token for auth and X-Gitlab-Event for type
            let token = headers
                .get(headers::GITLAB_TOKEN)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let evt = headers
                .get(headers::GITLAB_EVENT)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (token, evt)
        }
    };

    match state
        .webhook_handler
        .handle_webhook(&token_header, &event_header, &body)
        .await
    {
        Ok(_) => StatusCode::OK,
        Err(webhook::WebhookError::Unauthorized(msg)) => {
            tracing::warn!(reason = %msg, "webhook authentication failed");
            StatusCode::UNAUTHORIZED
        }
        Err(webhook::WebhookError::BadRequest(msg)) => {
            tracing::warn!(reason = %msg, "webhook request parsing failed");
            StatusCode::BAD_REQUEST
        }
        Err(webhook::WebhookError::NoMatchingTrigger(msg)) => {
            tracing::debug!(reason = %msg, "no matching trigger for webhook event");
            StatusCode::OK
        }
        Err(webhook::WebhookError::InternalError(msg)) => {
            tracing::error!(reason = %msg, "internal dispatcher error");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

/// Build the axum Router with all routes and middleware.
fn build_router(state: AppState, config: &ServerConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/webhook", post(webhook_handler))
        .with_state(state)
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(config.max_body_size as usize))
}

/// Run the HTTP server bound to the configured host:port.
///
/// Spawns a background dispatcher task that consumes events from the mpsc
/// channel and manages concurrency, deduplication, and persistence.
/// Graceful shutdown is handled via a `watch` channel — when a SIGINT or
/// SIGTERM signal is received, the signal handler sends `true` on the
/// watch channel, the HTTP server stops accepting new connections, and
/// the dispatcher drains in-flight workflows before exiting.
///
/// # Arguments
///
/// * `config` — Server configuration (host, port, webhook secret, etc.)
/// * `platform` — The platform type (GitHub or GitLab)
/// * `max_concurrent` — Maximum concurrent workflows (0 = unlimited)
/// * `workdir` — Directory for persisting dispatcher state
/// * `drain_timeout` — Maximum time to wait for in-flight workflows to complete
/// * `shutdown_rx` — Watch channel receiver that signals graceful shutdown
#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    config: &ServerConfig,
    platform: &Platform,
    max_concurrent: usize,
    workdir: PathBuf,
    drain_timeout: Duration,
    shutdown_rx: watch::Receiver<bool>,
    workflow_state: Arc<WorkflowState>,
    agents: Vec<AgentConfig>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| {
            format!(
                "Invalid host:port configuration ({}:{}): {e}",
                config.host, config.port
            )
        })?;

    let (tx, rx) = tokio::sync::mpsc::channel(100);

    let dedup_sets = new_dedup_sets();
    let dispatcher = Dispatcher::new(dedup_sets, max_concurrent, workdir, workflow_state, agents);

    // Spawn dispatcher run loop as a background task, passing drain_timeout
    let dispatcher_handle = tokio::spawn({
        let dispatcher = dispatcher.clone();
        let mut shutdown = shutdown_rx.clone();
        async move {
            dispatcher
                .run_with_drain(rx, &mut shutdown, drain_timeout)
                .await;
        }
    });

    let state = AppState {
        webhook_handler: webhook::WebhookHandler::new(
            platform.clone(),
            config.webhook_secret.clone(),
            tx,
        ),
        dispatcher: dispatcher.clone(),
    };

    let router = build_router(state, config);

    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Run server with graceful shutdown — watch for shutdown signal
    let shutdown_watch = async move {
        let mut rx = shutdown_rx;
        // Wait for the shutdown signal (value becomes true)
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            if *rx.borrow() {
                tracing::info!("HTTP server shutting down...");
                dispatcher.mark_shutting_down();
                break;
            }
        }
    };

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_watch)
        .await?;

    // Wait for dispatcher to finish draining
    dispatcher_handle.await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Platform;
    use crate::dispatcher::DispatchMessage;
    use crate::webhook::WebhookHandler;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use tokio::sync::mpsc;
    use tower::ServiceExt;

    type HmacSha256 = Hmac<Sha256>;

    fn test_workflow_state() -> std::sync::Arc<crate::reload::WorkflowState> {
        std::sync::Arc::new(crate::reload::WorkflowState::new(vec![]))
    }

    fn test_agents() -> Vec<AgentConfig> {
        vec![]
    }

    fn test_config() -> ServerConfig {
        ServerConfig {
            host: "0.0.0.0".to_string(),
            webhook_host: "yoke.example.com".to_string(),
            port: 0, // not used for in-memory tests
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1_048_576,
        }
    }

    fn test_state() -> (AppState, mpsc::Receiver<DispatchMessage>) {
        let (tx, rx) = mpsc::channel(100);
        let dedup_sets = crate::dispatcher::new_dedup_sets();
        let dispatcher = crate::dispatcher::Dispatcher::new(
            dedup_sets,
            0,
            PathBuf::from("/tmp/yoke-test"),
            test_workflow_state(),
            test_agents(),
        );
        let state = AppState {
            webhook_handler: WebhookHandler::new(Platform::Gitlab, "test-secret".to_string(), tx),
            dispatcher,
        };
        (state, rx)
    }

    fn test_state_github() -> (AppState, mpsc::Receiver<DispatchMessage>) {
        let (tx, rx) = mpsc::channel(100);
        let dedup_sets = crate::dispatcher::new_dedup_sets();
        let dispatcher = crate::dispatcher::Dispatcher::new(
            dedup_sets,
            0,
            PathBuf::from("/tmp/yoke-test"),
            test_workflow_state(),
            test_agents(),
        );
        let state = AppState {
            webhook_handler: WebhookHandler::new(Platform::Github, "test-secret".to_string(), tx),
            dispatcher,
        };
        (state, rx)
    }

    fn compute_signature(payload: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(result))
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn test_ready_endpoint() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    // --- GitLab webhook tests ---

    #[tokio::test]
    async fn test_webhook_gitlab_valid_token() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let issue_payload = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 42,
                "action": "update",
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITLAB_TOKEN, "test-secret")
                    .header(headers::GITLAB_EVENT, "Issue Hook")
                    .header("Content-Type", "application/json")
                    .body(Body::from(issue_payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_gitlab_invalid_token() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITLAB_TOKEN, "wrong-secret")
                    .header(headers::GITLAB_EVENT, "Issue Hook")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"object_kind":"issue","object_attributes":{"id":1},"project":{"id":1,"path_with_namespace":"o/r"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_webhook_gitlab_no_token_header() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"object_kind":"issue","object_attributes":{"id":1},"project":{"id":1,"path_with_namespace":"o/r"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Empty token header should fail verification
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_webhook_gitlab_no_matching_trigger() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        // "open" action on an issue doesn't map to any trigger
        let payload = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 42,
                "action": "open",
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITLAB_TOKEN, "test-secret")
                    .header(headers::GITLAB_EVENT, "Issue Hook")
                    .header("Content-Type", "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // No matching trigger is a 200 no-op, not an error
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_gitlab_bad_json() {
        let config = test_config();
        let (state, _rx) = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITLAB_TOKEN, "test-secret")
                    .header(headers::GITLAB_EVENT, "Issue Hook")
                    .header("Content-Type", "application/json")
                    .body(Body::from("not json"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // --- GitHub webhook tests ---

    #[tokio::test]
    async fn test_webhook_github_missing_signature_returns_401() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{"action": "assigned"}"#;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_EVENT, "issues")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_webhook_github_invalid_signature_returns_401() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{"action": "assigned"}"#;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, "sha256=bad_signature")
                    .header(headers::GITHUB_EVENT, "issues")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn test_webhook_github_valid_signature_unknown_event_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{}"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "push")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Unknown event types return 200 (no-op, don't want platform retries)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_github_issues_assigned_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{
            "action": "assigned",
            "issue": {
                "number": 42,
                "title": "Bug report",
                "body": "Something is broken",
                "assignee": {"login": "alice"},
                "assignees": [{"login": "alice"}]
            },
            "sender": {"login": "bob"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "issues")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_github_issue_comment_created_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{
            "action": "created",
            "comment": {
                "id": 12345,
                "body": "@alice please review"
            },
            "issue": {
                "number": 42,
                "title": "Some issue",
                "assignees": []
            },
            "sender": {"login": "charlie"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "issue_comment")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_github_pull_request_review_submitted_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{
            "action": "submitted",
            "review": {
                "id": 999,
                "body": "Looks good",
                "user": {"login": "reviewer"}
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "pull_request_review")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_github_pull_request_review_comment_created_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        let body = r#"{
            "action": "created",
            "comment": {
                "id": 555,
                "body": "Nit: fix typo",
                "pull_request_review_id": 999
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "commenter"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "pull_request_review_comment")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_webhook_github_no_matching_trigger_returns_200() {
        let config = test_config();
        let (state, _rx) = test_state_github();
        let app = build_router(state, &config);

        // "issues" event with action "opened" doesn't match any trigger
        let body = r#"{
            "action": "opened",
            "issue": {
                "number": 42,
                "title": "Bug report",
                "assignees": []
            },
            "sender": {"login": "bob"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITHUB_SIGNATURE, sig)
                    .header(headers::GITHUB_EVENT, "issues")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        // No matching trigger = 200 (no-op, platform doesn't need to retry)
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_body_limit_rejection() {
        // Use a very small body limit (10 bytes) to test rejection
        let config = ServerConfig {
            host: "0.0.0.0".to_string(),
            webhook_host: "yoke.example.com".to_string(),
            port: 0,
            webhook_secret: "test-secret".to_string(),
            max_body_size: 10,
        };
        let (state, _rx) = test_state();

        // Build router with DefaultBodyLimit for the rejection test,
        // since RequestBodyLimitLayer only rejects on body consumption.
        // We use a handler that actually reads the body to trigger the limit.
        let app = Router::new()
            .route(
                "/webhook",
                post(|body: axum::body::Bytes| async move {
                    let _ = body;
                    "OK"
                }),
            )
            .with_state(state)
            .layer(TraceLayer::new_for_http())
            .layer(RequestBodyLimitLayer::new(config.max_body_size as usize));

        // Send a body larger than the limit
        let large_body = vec![0u8; 100];
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("content-type", "application/json")
                    .body(Body::from(large_body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    /// When the dispatcher channel receiver is dropped (closed channel),
    /// the server should return 503 Service Unavailable.
    /// This simulates a dispatcher that has shut down or is unreachable.
    #[tokio::test]
    async fn test_webhook_gitlab_returns_503_when_channel_closed() {
        let config = test_config();
        // Create a channel, then drop rx to simulate a closed dispatcher channel.
        let (tx, rx) = mpsc::channel::<DispatchMessage>(1);
        drop(rx);

        let state = AppState {
            webhook_handler: WebhookHandler::new(Platform::Gitlab, "test-secret".to_string(), tx),
            dispatcher: crate::dispatcher::Dispatcher::new(
                crate::dispatcher::new_dedup_sets(),
                0,
                PathBuf::from("/tmp/yoke-test"),
                test_workflow_state(),
                test_agents(),
            ),
        };
        let app = build_router(state, &config);

        // Use a valid issue payload so dispatch_webhook returns a TriggerEvent,
        // which then hits the closed channel on send().
        let issue_payload = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 42,
                "action": "update",
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header(headers::GITLAB_TOKEN, "test-secret")
                    .header(headers::GITLAB_EVENT, "Issue Hook")
                    .header("content-type", "application/json")
                    .body(Body::from(issue_payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
