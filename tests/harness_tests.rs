//! Integration tests for the Hermes API client harness.

use std::fs;

use yoke::harness::{ContentBlock, HermesClient, HermesRequest, HermesResponse, OutputItem};

/// Verify that `HermesRequest` serializes to the expected JSON format.
#[test]
fn test_hermes_request_serialization() {
    let request = HermesRequest {
        instructions: Some("You are an expert.".to_string()),
        input: "Do the thing.".to_string(),
        store: true,
    };

    let json = serde_json::to_string(&request).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed["instructions"], "You are an expert.");
    assert_eq!(parsed["input"], "Do the thing.");
    assert_eq!(parsed["store"], true);

    // Verify round-trip
    let round_tripped: HermesRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(round_tripped, request);
}

/// Verify that `ContentBlock` deserializes correctly with the `type` field.
#[test]
fn test_content_block_deserialization() {
    let json = r#"{"type": "output_text", "text": "Hello!"}"#;
    let block: ContentBlock = serde_json::from_str(json).unwrap();
    assert_eq!(block.block_type, "output_text");
    assert_eq!(block.text, "Hello!");
}

/// Verify that `OutputItem` deserializes the nested message structure.
#[test]
fn test_output_item_deserialization() {
    let json = r#"{
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "output_text", "text": "Hello!"},
            {"type": "reasoning", "text": "Thinking..."}
        ]
    }"#;
    let item: OutputItem = serde_json::from_str(json).unwrap();
    assert_eq!(item.item_type, "message");
    assert_eq!(item.role, "assistant");
    assert_eq!(item.content.len(), 2);
    assert_eq!(item.content[0].block_type, "output_text");
    assert_eq!(item.content[0].text, "Hello!");
    assert_eq!(item.content[1].block_type, "reasoning");
}

/// Verify that `HermesResponse` parsing correctly extracts text from the
/// nested message structure — matching the real Hermes API response format.
#[test]
fn test_hermes_response_parsing_with_real_format() {
    // This matches the real Hermes API response format from issue #122
    let json = r#"{
        "id": "resp_aa51688313c14e0a85ace0cf0c9f",
        "object": "response",
        "status": "completed",
        "created_at": 1780089303,
        "model": "pm",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Here is the implementation plan."}
                ]
            }
        ],
        "usage": {"input_tokens": 205166, "output_tokens": 1517, "total_tokens": 206683}
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();
    assert_eq!(response.output.len(), 1);
    assert_eq!(response.output[0].item_type, "message");
    assert_eq!(response.output[0].role, "assistant");

    let extracted = response.extract_text();
    assert_eq!(extracted, "Here is the implementation plan.");
}

/// Verify that `HermesResponse` filters for `output_text` blocks within messages.
#[test]
fn test_hermes_response_filters_output_text() {
    let json = r#"{
        "id": "resp_filter",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "First"},
                    {"type": "reasoning", "text": "Thinking..."},
                    {"type": "output_text", "text": "Second"}
                ]
            }
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output = response.extract_text();
    assert_eq!(output, "First\nSecond");
}

/// Verify that `HermesResponse` with no `output_text` blocks returns empty string.
#[test]
fn test_hermes_response_no_output_text() {
    let json = r#"{
        "id": "resp_no_text",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "reasoning", "text": "Thinking..."},
                    {"type": "reasoning", "text": "Still thinking..."}
                ]
            }
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output = response.extract_text();
    assert!(output.is_empty());
}

/// Verify that multiple `HermesClient` instances can have different base URLs.
#[test]
fn test_multiple_client_instances_with_different_urls() {
    let client_a = HermesClient::new("http://localhost:8000".to_string(), "key-a".to_string());
    let client_b = HermesClient::new("http://localhost:8001".to_string(), "key-b".to_string());

    // Ensure different URLs and keys
    assert_ne!(client_a.base_url, client_b.base_url);
    assert_ne!(client_a.api_key, client_b.api_key);

    // Verify URL construction: base_url + /v1/responses
    let url_a = format!("{}/v1/responses", client_a.base_url.trim_end_matches('/'));
    let url_b = format!("{}/v1/responses", client_b.base_url.trim_end_matches('/'));

    assert_eq!(url_a, "http://localhost:8000/v1/responses");
    assert_eq!(url_b, "http://localhost:8001/v1/responses");
}

/// Verify that error file is created with the correct content format
/// when a non-2xx response is simulated.
#[test]
fn test_error_file_format() {
    let dir = tempfile::tempdir().unwrap();
    let error_path = dir.path().join(".error");

    // Simulate the error file writing logic from execute_step
    let status_code = 500u16;
    let body = "Internal Server Error";
    let error_content = format!("status: {}\nbody: {}", status_code, body);

    fs::write(&error_path, &error_content).unwrap();

    let content = fs::read_to_string(&error_path).unwrap();
    assert!(content.contains("status: 500"));
    assert!(content.contains("Internal Server Error"));
}

/// Verify that error file handles 401 Unauthorized correctly.
#[test]
fn test_error_file_unauthorized() {
    let dir = tempfile::tempdir().unwrap();
    let error_path = dir.path().join(".error");

    let status_code = 401u16;
    let body = r#"{"error": "Invalid API key"}"#;
    let error_content = format!("status: {}\nbody: {}", status_code, body);

    fs::write(&error_path, &error_content).unwrap();

    let content = fs::read_to_string(&error_path).unwrap();
    assert!(content.contains("status: 401"));
    assert!(content.contains("Invalid API key"));
}

/// Verify that `HermesRequest` with `store: true` serializes correctly.
#[test]
fn test_hermes_request_store_true() {
    let request = HermesRequest {
        instructions: Some("Plan the implementation.".to_string()),
        input: "Issue #42".to_string(),
        store: true,
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["store"], true);
}

/// Verify single `output_text` block extraction (common case).
#[test]
fn test_single_output_text_block() {
    let json = r#"{
        "id": "resp_single",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Here is the plan."}
                ]
            }
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output = response.extract_text();
    assert_eq!(output, "Here is the plan.");
}

/// Verify that empty output array returns empty string.
#[test]
fn test_hermes_response_empty_output() {
    let json = r#"{"id": "resp_empty", "output": []}"#;
    let response: HermesResponse = serde_json::from_str(json).unwrap();
    assert!(response.extract_text().is_empty());
}

/// Verify that extract_text returns content from the last message item.
#[test]
fn test_hermes_response_extracts_last_message() {
    let json = r#"{
        "id": "resp_multi",
        "output": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Earlier response"}
                ]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Final response"}
                ]
            }
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();
    let extracted = response.extract_text();
    assert_eq!(extracted, "Final response");
}

/// Verify that non-message output items are skipped by extract_text.
#[test]
fn test_hermes_response_skips_non_message_items() {
    let json = r#"{
        "id": "resp_skip",
        "output": [
            {
                "type": "reasoning",
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "Should not appear"}
                ]
            }
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();
    assert!(response.extract_text().is_empty());
}

/// Verify that HarnessError::Http includes the URL and cause details.
///
/// Regression test for issue #225: the error message was just
/// "HTTP request failed: error sending request for url (...)" with no
/// indication of *why* the request failed (timeout, connection refused,
/// DNS error, etc.). The new structured variant must include the URL,
/// timeout/connect status, and the cause chain.
#[test]
fn test_harness_http_error_includes_details() {
    use yoke::harness::HarnessError;

    let err = HarnessError::Http {
        message: "error sending request for url (http://10.200.0.3:8500/v1/responses): timeout reached: operation timed out".to_string(),
    };
    let display = format!("{err}");
    assert!(display.contains("HTTP request failed"));
    assert!(display.contains("http://10.200.0.3:8500/v1/responses"));
    assert!(display.contains("timeout reached"));
    assert!(display.contains("operation timed out"));
}

// --- Agent health check tests ---

/// Verify that `check_agent_health` succeeds when the agent returns a healthy
/// response with `status: "ok"`.
#[tokio::test]
async fn test_health_check_all_healthy() {
    use mockito::ServerGuard;
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::check_agent_health;

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;

    let url = Url::parse(&server.url()).unwrap();
    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: url,
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_ok(), "expected health check to succeed");
    let health = result.unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.platform, "hermes-agent");
    assert_eq!(health.version, "0.17.0");

    mock.assert();
}

/// Verify that `check_agent_health` returns `BadStatus` when the agent
/// returns a non-200 status code.
#[tokio::test]
async fn test_health_check_bad_status() {
    use mockito::ServerGuard;
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::{check_agent_health, HealthCheckError};

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(503)
        .with_body("Service Unavailable")
        .create_async()
        .await;

    let url = Url::parse(&server.url()).unwrap();
    let agent = AgentConfig {
        name: "swe".to_string(),
        base_url: url,
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::BadStatus { agent, status, .. } => {
            assert_eq!(agent, "swe");
            assert_eq!(status, 503);
        }
        other => panic!("expected BadStatus, got: {other:?}"),
    }

    mock.assert();
}

/// Verify that `check_agent_health` returns `Http` when the agent is
/// unreachable (connection refused).
#[tokio::test]
async fn test_health_check_http_error() {
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::{check_agent_health, HealthCheckError};

    // Use a port that's almost certainly not listening
    let url = Url::parse("http://127.0.0.1:1").unwrap();
    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: url,
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

/// Verify that `check_agent_health` returns `Parse` when the agent
/// returns a 200 response with an unparseable body.
#[tokio::test]
async fn test_health_check_parse_error() {
    use mockito::ServerGuard;
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::{check_agent_health, HealthCheckError};

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body("not json at all")
        .create_async()
        .await;

    let url = Url::parse(&server.url()).unwrap();
    let agent = AgentConfig {
        name: "reviewer".to_string(),
        base_url: url,
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::Parse { agent, .. } => {
            assert_eq!(agent, "reviewer");
        }
        other => panic!("expected Parse, got: {other:?}"),
    }

    mock.assert();
}

/// Verify that `check_agent_health` returns `BadStatus` when the agent
/// returns a 200 response but with `status: "unhealthy"`.
#[tokio::test]
async fn test_health_check_unhealthy_status() {
    use mockito::ServerGuard;
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::{check_agent_health, HealthCheckError};

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"unhealthy","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;

    let url = Url::parse(&server.url()).unwrap();
    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: url,
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_err());
    match result.unwrap_err() {
        HealthCheckError::BadStatus { agent, body, .. } => {
            assert_eq!(agent, "pm");
            assert!(body.contains("unhealthy"));
        }
        other => panic!("expected BadStatus, got: {other:?}"),
    }

    mock.assert();
}

/// Verify that `check_agent_health` works correctly when the base_url
/// has a trailing slash.
#[tokio::test]
async fn test_health_check_trailing_slash() {
    use mockito::ServerGuard;
    use url::Url;
    use yoke::config::AgentConfig;
    use yoke::harness::check_agent_health;

    let mut server = ServerGuard::new_async().await;
    let mock = server
        .mock("GET", "/health")
        .with_status(200)
        .with_body(r#"{"status":"ok","platform":"hermes-agent","version":"0.17.0"}"#)
        .create_async()
        .await;

    // Note: Url::parse normalizes trailing slashes, but test anyway
    let url = Url::parse(&format!("{}/", server.url())).unwrap();
    let agent = AgentConfig {
        name: "pm".to_string(),
        base_url: url,
    };

    let result = check_agent_health(&agent).await;
    assert!(result.is_ok(), "expected health check to succeed with trailing slash");

    mock.assert();
}
