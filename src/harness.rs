//! Hermes API client harness for making agent requests and parsing responses.
//!
//! This module provides a high-level HTTP client (`HermesClient`) that:
//! - Sends POST requests to the `/v1/responses` endpoint of a Hermes Agent API
//! - Authenticates via `HERMES_API_KEY` as a Bearer token
//! - Builds request payloads with `instructions` (optional), `input`, and `store` fields
//! - Parses responses to extract `output_text` content blocks
//! - Writes non-2xx error details to a `.error` file in the current directory

use std::fs;
use std::path::Path;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Request body sent to the Hermes API `/v1/responses` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HermesRequest {
    /// The system-level instructions for the agent.
    ///
    /// When present, this is sent as the `instructions` field in the API request.
    /// When `None`, the field is omitted from the JSON payload entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The user input / prompt for the agent.
    pub input: String,
    /// Whether to persist the conversation on the server side.
    pub store: bool,
}

/// A single content block within a message's `content` array.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContentBlock {
    /// The type of content block (e.g. `"output_text"`).
    #[serde(rename = "type")]
    pub block_type: String,
    /// The text content of the block.
    #[serde(default)]
    pub text: String,
}

/// An output item in the Hermes API response.
///
/// The Hermes `/v1/responses` endpoint returns `output` as an array of items.
/// Items with `type: "message"` carry the assistant's response in their
/// `content` field, which is an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputItem {
    /// The item type (e.g. `"message"`).
    #[serde(rename = "type")]
    pub item_type: String,
    /// The role of the message author (e.g. `"assistant"`).
    #[serde(default)]
    pub role: String,
    /// Content blocks within this output item.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

/// The top-level response from the Hermes API `/v1/responses` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HermesResponse {
    /// Output items returned by the agent.
    ///
    /// Each item is typically a `"message"` object containing a `content`
    /// array of content blocks. The last message in the output contains
    /// the final assistant response.
    pub output: Vec<OutputItem>,
}

impl HermesResponse {
    /// Extract all `output_text` content from the last message in the output.
    ///
    /// The Hermes API returns multiple output items; the last one with
    /// `type: "message"` contains the assistant's final response.
    /// This method finds that last message and concatenates all
    /// `output_text` blocks within it, separated by newlines.
    pub fn extract_text(&self) -> String {
        self.output
            .iter()
            .rev()
            .find(|item| item.item_type == "message")
            .map(|msg| {
                msg.content
                    .iter()
                    .filter(|block| block.block_type == "output_text")
                    .map(|block| block.text.as_str())
                    .collect::<Vec<&str>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

/// The result of executing a single agent step.
///
/// Contains the extracted message text plus the raw HTTP exchange data
/// for audit logging (`.prompt` and `.log` files).
#[derive(Debug, Clone)]
pub struct StepResult {
    /// The extracted output text from `output_text` content blocks.
    pub extracted_message: String,
    /// The raw JSON request body sent to the API.
    pub raw_request: String,
    /// The raw JSON response body received from the API.
    pub raw_response: String,
}

/// Errors that can occur during harness operations.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// HTTP request failed (network or server error).
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The API returned a non-2xx status code.
    #[error("API error {status}: {body}")]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The response body text.
        body: String,
    },
    /// An I/O error occurred writing the `.error` file.
    #[error("IO error writing .error file: {0}")]
    Io(#[from] std::io::Error),
}

/// HTTP client for the Hermes Agent API.
///
/// Encapsulates the base URL, API key, and `reqwest::Client` for making
/// authenticated requests to the `/v1/responses` endpoint.
#[derive(Debug, Clone)]
pub struct HermesClient {
    /// The base URL of the Hermes Agent API (e.g. `http://localhost:8000`).
    pub base_url: String,
    /// The API key for Bearer token authentication.
    pub api_key: String,
    client: Client,
}

impl HermesClient {
    /// Create a new `HermesClient` with the given base URL and API key.
    ///
    /// The `base_url` should be the host-only URL without a trailing slash
    /// (e.g. `http://localhost:8000`). The `/v1/responses` path is appended
    /// internally by `execute_step`.
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            base_url,
            api_key,
            client: Client::new(),
        }
    }

    /// Execute a single agent step by sending a request to the Hermes API.
    ///
    /// 1. Builds a `HermesRequest` from the given `instructions` (optional) and `input`.
    /// 2. Sends a POST to `{base_url}/v1/responses` with Bearer token auth.
    /// 3. On success, parses the response and extracts `output_text` blocks.
    /// 4. On failure (non-2xx), writes status and body to a `.error` file
    ///    and returns a `HarnessError::Api`.
    ///
    /// Returns a `StepResult` containing the extracted message, raw request
    /// body, and raw response body for audit logging.
    pub async fn execute_step(
        &self,
        instructions: Option<&str>,
        input: &str,
    ) -> Result<StepResult, HarnessError> {
        self.execute_step_with_error_path(instructions, input, None)
            .await
    }

    /// Execute a step with an explicit path for the `.error` file.
    ///
    /// This is primarily useful for testing where the error file location
    /// needs to be controlled.
    ///
    /// Returns a `StepResult` containing the extracted message, raw request
    /// body, and raw response body for audit logging.
    pub async fn execute_step_with_error_path(
        &self,
        instructions: Option<&str>,
        input: &str,
        error_path: Option<&Path>,
    ) -> Result<StepResult, HarnessError> {
        let request = HermesRequest {
            instructions: instructions.map(|s| s.to_string()),
            input: input.to_string(),
            store: true,
        };

        let raw_request = serde_json::to_string_pretty(&request)
            .unwrap_or_else(|_| serde_json::to_string(&request).unwrap_or_default());

        let url = format!("{}/v1/responses", self.base_url.trim_end_matches('/'));

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        let raw_response = response.text().await?;

        if !status.is_success() {
            let error_path = error_path.unwrap_or(Path::new(".error"));
            let error_content = format!("status: {}\nbody: {}", status.as_u16(), raw_response);
            fs::write(error_path, &error_content)?;

            return Err(HarnessError::Api {
                status: status.as_u16(),
                body: raw_response,
            });
        }

        let hermes_response: HermesResponse =
            serde_json::from_str(&raw_response).map_err(|e| HarnessError::Api {
                status: 200,
                body: format!("Failed to parse response JSON: {e}"),
            })?;

        let extracted_message = hermes_response.extract_text();

        Ok(StepResult {
            extracted_message,
            raw_request,
            raw_response,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hermes_request_serialization() {
        let request = HermesRequest {
            instructions: Some("You are an expert software engineer.".to_string()),
            input: "Fix the bug in main.rs".to_string(),
            store: true,
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed["instructions"],
            "You are an expert software engineer."
        );
        assert_eq!(parsed["input"], "Fix the bug in main.rs");
        assert_eq!(parsed["store"], true);
    }

    #[test]
    fn test_hermes_request_instructions_omitted_when_none() {
        let request = HermesRequest {
            instructions: None,
            input: "Fix the bug in main.rs".to_string(),
            store: true,
        };

        let json = serde_json::to_string(&request).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // When instructions is None, the field should not appear in JSON
        assert!(parsed.get("instructions").is_none());
        assert_eq!(parsed["input"], "Fix the bug in main.rs");
        assert_eq!(parsed["store"], true);
    }

    #[test]
    fn test_content_block_deserialization() {
        let json = r#"{"type": "output_text", "text": "Hello, world!"}"#;
        let block: ContentBlock = serde_json::from_str(json).unwrap();
        assert_eq!(block.block_type, "output_text");
        assert_eq!(block.text, "Hello, world!");
    }

    #[test]
    fn test_output_item_deserialization() {
        let json = r#"{
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "Hello!"}
            ]
        }"#;
        let item: OutputItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.item_type, "message");
        assert_eq!(item.role, "assistant");
        assert_eq!(item.content.len(), 1);
        assert_eq!(item.content[0].block_type, "output_text");
        assert_eq!(item.content[0].text, "Hello!");
    }

    #[test]
    fn test_hermes_response_parsing_with_nested_output() {
        let json = r#"{
            "id": "resp_test123",
            "object": "response",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "First message"},
                        {"type": "reasoning", "text": "Thinking..."},
                        {"type": "output_text", "text": "Second message"}
                    ]
                }
            ]
        }"#;

        let response: HermesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.output.len(), 1);
        assert_eq!(response.output[0].item_type, "message");
        assert_eq!(response.output[0].content.len(), 3);

        let extracted = response.extract_text();
        assert_eq!(extracted, "First message\nSecond message");
    }

    #[test]
    fn test_hermes_response_extracts_last_message() {
        let json = r#"{
            "id": "resp_test456",
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

    #[test]
    fn test_hermes_response_no_message_output() {
        let json = r#"{
            "id": "resp_test789",
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

    #[test]
    fn test_hermes_response_empty_output() {
        let json = r#"{"id": "resp_empty", "output": []}"#;
        let response: HermesResponse = serde_json::from_str(json).unwrap();
        assert!(response.extract_text().is_empty());
    }

    #[test]
    fn test_step_result_fields() {
        let result = StepResult {
            extracted_message: "Hello".to_string(),
            raw_request: r#"{"instructions":"test","input":"","store":true}"#.to_string(),
            raw_response: r#"{"id":"resp_x","output":[]}"#.to_string(),
        };
        assert_eq!(result.extracted_message, "Hello");
        assert_eq!(
            result.raw_request,
            r#"{"instructions":"test","input":"","store":true}"#
        );
        assert_eq!(result.raw_response, r#"{"id":"resp_x","output":[]}"#);
    }

    #[test]
    fn test_hermes_client_new() {
        let client = HermesClient::new(
            "http://localhost:8000".to_string(),
            "test-api-key".to_string(),
        );
        assert_eq!(client.base_url, "http://localhost:8000");
        assert_eq!(client.api_key, "test-api-key");
    }

    #[test]
    fn test_hermes_client_different_base_urls() {
        let client_a = HermesClient::new("http://localhost:8000".to_string(), "key-a".to_string());
        let client_b = HermesClient::new("http://localhost:8001".to_string(), "key-b".to_string());

        assert_ne!(client_a.base_url, client_b.base_url);
        assert_ne!(client_a.api_key, client_b.api_key);

        // Verify URL construction
        let url_a = format!("{}/v1/responses", client_a.base_url.trim_end_matches('/'));
        let url_b = format!("{}/v1/responses", client_b.base_url.trim_end_matches('/'));

        assert_eq!(url_a, "http://localhost:8000/v1/responses");
        assert_eq!(url_b, "http://localhost:8001/v1/responses");
    }

    #[test]
    fn test_error_file_created_on_non_2xx() {
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let error_path = dir.path().join(".error");

        // Simulate what execute_step does on non-2xx:
        // Write status and body to .error file
        let status_code = 500u16;
        let body = "Internal Server Error";
        let error_content = format!("status: {}\nbody: {}", status_code, body);
        fs::write(&error_path, &error_content).unwrap();

        // Verify the file was created with the right content
        let content = fs::read_to_string(&error_path).unwrap();
        assert!(content.contains("status: 500"));
        assert!(content.contains("Internal Server Error"));
    }

    #[test]
    fn test_harness_error_display() {
        let err = HarnessError::Api {
            status: 500,
            body: "Internal Server Error".to_string(),
        };
        assert_eq!(format!("{err}"), "API error 500: Internal Server Error");

        let io_err = HarnessError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        assert!(format!("{io_err}").contains("file not found"));
    }
}
