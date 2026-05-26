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

use crate::config::ServerConfig;
use crate::webhook::github::{
    GitHubWebhookError, map_to_trigger_event, parse_github_event, verify_github_signature,
};

/// Application state shared across handlers.
#[derive(Clone)]
pub struct AppState {
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

/// GitHub webhook handler — verifies HMAC-SHA256 signature, parses the event,
/// and maps it to an internal trigger type.
///
/// Returns:
/// - `401` if the signature is missing, malformed, or doesn't match
/// - `400` if the event type header is missing or the payload is invalid
/// - `200` if the event is processed (even if no trigger matched)
///   or if the event type is unknown (no-op per architecture spec)
async fn github_webhook_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> StatusCode {
    // Extract X-Hub-Signature-256 header
    let signature = match headers.get("X-Hub-Signature-256") {
        Some(v) => match v.to_str() {
            Ok(s) => s,
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

    // Verify HMAC signature
    if let Err(e) = verify_github_signature(&body, signature, &state.webhook_secret) {
        tracing::warn!(error = %e, "webhook signature verification failed");
        return StatusCode::UNAUTHORIZED;
    }

    // Extract X-GitHub-Event header
    let event_type = match headers.get("X-GitHub-Event") {
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

    // Parse the event payload
    let event = match parse_github_event(&event_type, &body) {
        Ok(e) => e,
        Err(GitHubWebhookError::UnknownEventType(t)) => {
            // Unknown event types are a no-op — return 200 so the platform doesn't retry
            tracing::debug!(event_type = %t, "unhandled event type, ignoring");
            return StatusCode::OK;
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to parse webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    // Map the event to an internal trigger
    // owner and repo are extracted from the event payload in a future issue
    // when we wire up repository filtering. For now, use placeholder values.
    let owner = "";
    let repo = "";

    match map_to_trigger_event(&event, owner, repo) {
        Some(trigger) => {
            tracing::info!(
                event_type = %event.event_type,
                action = %event.action,
                trigger_type = ?trigger.trigger_type,
                event_id = %trigger.event_id,
                "webhook event mapped to trigger"
            );
            // TODO: Send trigger to dispatcher channel (future issue)
        }
        None => {
            tracing::debug!(
                event_type = %event.event_type,
                action = %event.action,
                "no matching trigger for event"
            );
        }
    }

    StatusCode::OK
}

/// Build the axum Router with all routes and middleware.
fn build_router(config: &ServerConfig) -> Router {
    let state = AppState {
        webhook_secret: config.webhook_secret.clone(),
    };

    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/webhook", post(github_webhook_handler))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(config.max_body_size as usize))
        .with_state(state)
}

/// Run the HTTP server bound to the configured host:port.
///
/// Blocks until the server shuts down.
pub async fn run_server(
    config: &ServerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port)
        .parse()
        .map_err(|e| {
            format!(
                "Invalid host:port configuration ({}:{}): {e}",
                config.host, config.port
            )
        })?;

    let router = build_router(config);

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

    fn compute_signature(payload: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(result))
    }

    #[tokio::test]
    async fn test_health_endpoint() {
        let config = test_config();
        let app = build_router(&config);

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
        let app = build_router(&config);

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

    #[tokio::test]
    async fn test_webhook_missing_signature_returns_401() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_invalid_signature_returns_401() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_missing_event_type_returns_400() {
        let config = test_config();
        let app = build_router(&config);

        let body = r#"{"action": "assigned"}"#;
        let sig = compute_signature(body.as_bytes(), &config.webhook_secret);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .header("X-Hub-Signature-256", sig)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_webhook_valid_signature_unknown_event_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_issues_assigned_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_issue_comment_created_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_pull_request_review_submitted_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_pull_request_review_comment_created_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
    async fn test_webhook_no_matching_trigger_returns_200() {
        let config = test_config();
        let app = build_router(&config);

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
