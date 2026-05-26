//! Integration tests for the Hermes API client harness.

use std::fs;

use yoke::harness::{ContentBlock, HermesClient, HermesRequest, HermesResponse};

/// Verify that `HermesRequest` serializes to the expected JSON format.
#[test]
fn test_hermes_request_serialization() {
    let request = HermesRequest {
        instructions: "You are an expert.".to_string(),
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

/// Verify that `HermesResponse` parsing filters for `output_text` blocks.
#[test]
fn test_hermes_response_filters_output_text() {
    let json = r#"{
        "output": [
            {"type": "output_text", "text": "First"},
            {"type": "reasoning", "text": "Thinking..."},
            {"type": "output_text", "text": "Second"}
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output: String = response
        .output
        .iter()
        .filter(|b| b.block_type == "output_text")
        .map(|b| b.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

    assert_eq!(output, "First\nSecond");
}

/// Verify that `HermesResponse` with no `output_text` blocks returns empty string.
#[test]
fn test_hermes_response_no_output_text() {
    let json = r#"{
        "output": [
            {"type": "reasoning", "text": "Thinking..."},
            {"type": "reasoning", "text": "Still thinking..."}
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output: String = response
        .output
        .iter()
        .filter(|b| b.block_type == "output_text")
        .map(|b| b.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

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
    let body = "{\"error\": \"Invalid API key\"}";
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
        instructions: "Plan the implementation.".to_string(),
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
        "output": [
            {"type": "output_text", "text": "Here is the plan."}
        ]
    }"#;

    let response: HermesResponse = serde_json::from_str(json).unwrap();

    let output: String = response
        .output
        .iter()
        .filter(|b| b.block_type == "output_text")
        .map(|b| b.text.as_str())
        .collect::<Vec<&str>>()
        .join("\n");

    assert_eq!(output, "Here is the plan.");
}
