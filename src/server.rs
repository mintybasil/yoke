use axum::{
    Json, Router,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::net::SocketAddr;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::config::ServerConfig;

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

/// Webhook placeholder handler — always returns 200 OK.
/// Full webhook handling is implemented in a separate issue.
async fn webhook_placeholder() -> &'static str {
    "OK"
}

/// Build the axum Router with all routes and middleware.
fn build_router(config: &ServerConfig) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/webhook", post(webhook_placeholder))
        .layer(TraceLayer::new_for_http())
        .layer(RequestBodyLimitLayer::new(config.max_body_size as usize))
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
    use axum::extract::DefaultBodyLimit;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_config() -> ServerConfig {
        ServerConfig {
            host: "0.0.0.0".to_string(),
            port: 0, // not used for in-memory tests
            webhook_secret: "test-secret".to_string(),
            max_body_size: 1024, // 1 KB for tests
        }
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
    async fn test_webhook_placeholder() {
        let config = test_config();
        let app = build_router(&config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhook")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

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
