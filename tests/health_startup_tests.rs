//! Integration tests for the agent health check on startup.
//!
//! These tests verify that the `check_agent_health` function correctly
//! handles multiple agents and reports failures for unhealthy agents.

use url::Url;

use yoke::config::AgentConfig;
use yoke::harness::{HealthCheckError, check_agent_health};

/// Verify that health checks pass for multiple healthy agents.
#[tokio::test]
async fn test_multi_agent_all_healthy() {
    use mockito::ServerGuard;

    let mut server_a = ServerGuard::new_async().await;
    let mut server_b = ServerGuard::new_async().await;

    let mock_a = server_a
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;
    let mock_b = server_b
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.18.0"}"#)
        .create_async()
        .await;

    let agents = vec![
        AgentConfig {
            name: "pm".to_string(),
            base_url: Url::parse(&server_a.url()).unwrap(),
        },
        AgentConfig {
            name: "swe".to_string(),
            base_url: Url::parse(&server_b.url()).unwrap(),
        },
    ];

    for agent in &agents {
        let result = check_agent_health(agent).await;
        assert!(result.is_ok(), "agent '{}' should be healthy", agent.name);
    }

    mock_a.assert();
    mock_b.assert();
}

/// Verify that health check fails when one agent out of many is unhealthy.
#[tokio::test]
async fn test_multi_agent_one_unhealthy() {
    use mockito::ServerGuard;

    let mut server_a = ServerGuard::new_async().await;
    let mut server_b = ServerGuard::new_async().await;

    let mock_a = server_a
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;
    let mock_b = server_b
        .mock("GET", "/health")
        .with_status(503)
        .with_body("Service Unavailable")
        .create_async()
        .await;

    let agents = vec![
        AgentConfig {
            name: "pm".to_string(),
            base_url: Url::parse(&server_a.url()).unwrap(),
        },
        AgentConfig {
            name: "swe".to_string(),
            base_url: Url::parse(&server_b.url()).unwrap(),
        },
    ];

    // First agent should be healthy
    let result = check_agent_health(&agents[0]).await;
    assert!(result.is_ok());

    // Second agent should fail
    let result = check_agent_health(&agents[1]).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::BadStatus { agent, status, .. } => {
            assert_eq!(agent, "swe");
            assert_eq!(status, 503);
        }
        other => panic!("expected BadStatus, got: {other:?}"),
    }

    mock_a.assert();
    mock_b.assert();
}

/// Verify that health check fails when an agent returns a status that is
/// not "ok".
#[tokio::test]
async fn test_multi_agent_status_not_ok() {
    use mockito::ServerGuard;

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"degraded","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;

    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: Url::parse(&server.url()).unwrap(),
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::BadStatus { agent, body, .. } => {
            assert_eq!(agent, "pm");
            assert!(body.contains("degraded"));
        }
        other => panic!("expected BadStatus, got: {other:?}"),
    }

    mock.assert();
}

/// Verify that health check fails when an agent returns invalid JSON.
#[tokio::test]
async fn test_multi_agent_invalid_json() {
    use mockito::ServerGuard;

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body("<html>not json</html>")
        .create_async()
        .await;

    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: Url::parse(&server.url()).unwrap(),
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::Parse { agent, .. } => {
            assert_eq!(agent, "pm");
        }
        other => panic!("expected Parse, got: {other:?}"),
    }

    mock.assert();
}

/// Verify that health check fails with an HTTP error when the agent is
/// unreachable (connection refused).
#[tokio::test]
async fn test_multi_agent_connection_refused() {
    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: Url::parse("http://127.0.0.1:1").unwrap(),
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::Http { agent, .. } => {
            assert_eq!(agent, "pm");
        }
        other => panic!("expected Http, got: {other:?}"),
    }
}

/// Verify that the health check URL is constructed correctly by checking
/// that the mock server receives the request at `/health`.
#[tokio::test]
async fn test_health_check_url_construction() {
    use mockito::ServerGuard;

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;

    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: Url::parse(&server.url()).unwrap(),
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_ok());

    // Verify the mock was hit exactly once at /health
    mock.assert();
}
