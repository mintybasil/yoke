use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::config::{Platform, ServerConfig};
use crate::webhook;

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
    pub platform: Platform,
    pub webhook_secret: String,
}

/// HTTP server encapsulating an axum Router and bind address.
///
/// Will hold dispatcher state in a future issue.
#[allow(dead_code)]
pub struct Server {
    router: Router,
    addr: SocketAddr,
}

/// Health check handler — returns `{"status": "ok"}`.
async fn health() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

/// Readiness check handler — returns 200 when the server is accepting events.
/// Currently always ready; will be wired to dispatcher state in a later issue.
async fn ready() -> &'static str {
    "OK"
}

/// Webhook handler — dispatches to platform-specific verification and processing.
///
/// Extracts authentication and event-type headers, then delegates to
/// `webhook::dispatch_webhook` which routes to the GitHub or GitLab handler
/// based on the configured platform.
///
/// Returns:
/// - `200 OK` if the event was processed or no matching trigger was found (no-op)
/// - `401 Unauthorized` if signature/token verification fails
/// - `400 Bad Request` if the payload cannot be parsed
async fn webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    // Extract authentication header based on platform
    let (token_header, event_header) = match state.platform {
        Platform::Github => {
            // GitHub uses X-Hub-Signature-256 for HMAC and X-GitHub-Event for type
            let sig = match headers.get("X-Hub-Signature-256") {
                Some(v) => match v.to_str() {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        tracing::warn!("invalid X-Hub-Signature-256 header encoding");
                        return StatusCode::UNAUTHORIZED;
                    }
                },
                None => {
                    tracing::warn!("missing X-Hub-Signature-256 header");
                    return StatusCode::UNAUTHORIZED;
                }
            };
            let evt = match headers.get("X-GitHub-Event") {
                Some(v) => match v.to_str() {
                    Ok(s) => s.to_string(),
                    Err(_) => {
                        tracing::warn!("invalid X-GitHub-Event header encoding");
                        return StatusCode::BAD_REQUEST;
                    }
                },
                None => {
                    tracing::warn!("missing X-GitHub-Event header");
                    return StatusCode::BAD_REQUEST;
                }
            };
            (sig, evt)
        }
        Platform::Gitlab => {
            // GitLab uses X-Gitlab-Token for auth and X-Gitlab-Event for type
            let token = headers
                .get("X-Gitlab-Token")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let evt = headers
                .get("X-Gitlab-Event")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            (token, evt)
        }
    };

    match webhook::dispatch_webhook(
        &state.platform,
        &token_header,
        &event_header,
        &body,
        &state.webhook_secret,
    ) {
        Ok(trigger_event) => {
            tracing::info!(
                trigger_type = trigger_event.trigger_type.label(),
                repo_path = %trigger_event.repo_path,
                event_id = %trigger_event.event_id,
                "webhook event processed"
            );
            // TODO: Send trigger_event through dispatcher channel (future issue)
            StatusCode::OK
        }
        Err(webhook::WebhookError::Unauthorized(msg)) => {
            tracing::warn!(reason = %msg, "webhook authentication failed");
            StatusCode::UNAUTHORIZED
        }
        Err(webhook::WebhookError::BadRequest(msg)) => {
            tracing::warn!(reason = %msg, "webhook request parsing failed");
            StatusCode::BAD_REQUEST
        }
        Err(webhook::WebhookError::NoMatchingTrigger(msg)) => {
            // No matching trigger — this is a no-op, not an error.
            // The platform shouldn't retry, so we return 200.
            tracing::debug!(reason = %msg, "no matching trigger for webhook event");
            StatusCode::OK
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
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(config.max_body_size as usize))
}

/// Run the HTTP server bound to the configured host:port.
///
/// Blocks until the server shuts down.
pub async fn run_server(
    config: &ServerConfig,
    platform: &Platform,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| {
            format!(
                "Invalid host:port configuration ({}:{}): {e}",
                config.host, config.port
            )
        })?;

    let state = AppState {
        platform: platform.clone(),
        webhook_secret: config.webhook_secret.clone(),
    };

    let router = build_router(state, config);

    tracing::info!("Starting server on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use tower::ServiceExt;

    type HmacSha256 = Hmac<Sha256>;

    fn test_config() -> ServerConfig {
        ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 0, // not used for in-memory tests
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1_048_576,
        }
    }

    fn test_state() -> AppState {
        AppState {
            platform: Platform::Gitlab,
            webhook_secret: "test-secret".to_string(),
        }
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
        let state = test_state();
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
        let state = test_state();
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
        let state = test_state();
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
                    .header("X-Gitlab-Token", "test-secret")
                    .header("X-Gitlab-Event", "Issue Hook")
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
        let state = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Gitlab-Token", "wrong-secret")
                    .header("X-Gitlab-Event", "Issue Hook")
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
        let state = test_state();
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
        let state = test_state();
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
                    .header("X-Gitlab-Token", "test-secret")
                    .header("X-Gitlab-Event", "Issue Hook")
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
        let state = test_state();
        let app = build_router(state, &config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Gitlab-Token", "test-secret")
                    .header("X-Gitlab-Event", "Issue Hook")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
        let app = build_router(state, &config);

        let body = r#"{"action": "assigned"}"#;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-GitHub-Event", "issues")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
        let app = build_router(state, &config);

        let body = r#"{"action": "assigned"}"#;
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", "sha256=bad_signature")
                    .header("X-GitHub-Event", "issues")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
        let app = build_router(state, &config);

        let body = r#"{}"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "push")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
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
            "sender": {"login": "bob"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "issues")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
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
            "sender": {"login": "charlie"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "issue_comment")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
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
            "sender": {"login": "reviewer"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "pull_request_review")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
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
            "sender": {"login": "commenter"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "pull_request_review_comment")
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
        let state = AppState {
            platform: Platform::Github,
            webhook_secret: "test-secret".to_string(),
        };
        let app = build_router(state, &config);

        // "issues" event with action "opened" doesn't match any trigger
        let body = r#"{
            "action": "opened",
            "issue": {
                "number": 42,
                "title": "Bug report",
                "assignees": []
            },
            "sender": {"login": "bob"}
        }"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("X-GitHub-Event", "issues")
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
            port: 0,
            webhook_secret: "test-secret".to_string(),
            max_body_size: 10,
        };
        let state = test_state();

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
}
