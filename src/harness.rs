//! Hermes API client harness.
//!
//! This module provides a high-level HTTP client (`HermesClient`) for making
//! agent requests to a Hermes Agent API and parsing responses.
//!
//! Key behaviors:
//! - Sends POST requests to `/v1/responses` endpoint
//! - Authenticates via `HERMES_API_KEY` as a Bearer token
//! - Builds payloads with `instructions` (optional), `input`, and `store` fields
//! - Parses responses to extract `output_text` content blocks
//! - Writes non-2xx error details to a `.error` file in the current directory

use std::error::Error as StdError;
use std::fs;
use std::path::Path;

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Request body sent to the Hermes API `/v1/responses` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HermesRequest {
    /// The system-level instructions for the agent.
    ///
    /// When present, this is sent as the `instructions` field in the API request.
    /// When `None`, the field is omitted from the JSON payload entirely.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// The user input/prompt for the agent.
    pub input: String,
    /// Whether to persist the conversation server-side.
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

/// Items with `type: "message"` carry the assistant's response in their
/// `content` field, which is an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OutputItem {
    /// The item type (e.g. `"message"`).
    #[serde(rename = "type")]
    pub item_type: String,
    /// The author role (e.g. `"assistant"`).
    #[serde(default)]
    pub role: String,
    /// The content blocks within this item.
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

/// The top-level response from the Hermes API `/v1/responses` endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HermesResponse {
    /// Output items returned by the agent.
    ///
    /// Each item is typically a `"message"` object containing a `content`
    /// array of content blocks.
    pub output: Vec<OutputItem>,
}

impl HermesResponse {
    /// Extract the assistant's response text from the API response.
    ///
    /// Only the last output item with `type: "message"` contains the assistant's final response.
    /// This method finds that last message and concatenates all
    /// `output_text` blocks within it, separated by newlines.
    pub fn extract_text(&self) -> String {
        self.output
            .iter()
            .rev()
            .find(|item| item.item_type == "message")
            .map(|item| {
                item.content
                    .iter()
                    .filter(|block| block.block_type == "output_text")
                    .map(|block| block.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }
}

/// Contains the extracted message text plus the raw HTTP exchange data
/// for audit logging (`.prompt` and `.log` files).
#[derive(Debug, Clone)]
pub struct StepResult {
    /// The extracted output text from `output_text` content blocks.
    pub extracted_message: String,
    /// The raw JSON request body sent to the API.
    pub raw_request: String,
    /// The raw JSON response body received.
    pub raw_response: String,
}

/// Errors that can occur during harness operations.
#[derive(Debug, Error)]
pub enum HarnessError {
    /// HTTP request failed (network error, timeout, or server error).
    ///
    /// Carries structured details so the error message includes *why* the
    /// request failed (timeout, connection refused, DNS error, etc.) rather
    /// than only the opaque `error sending request for url (...)` string that
    /// `reqwest::Error` produces by default.
    #[error("HTTP request failed: {message}")]
    Http {
        /// Human-readable summary of the failure, including the URL,
        /// timeout/connect status, and the full cause chain.
        message: String,
    },
    /// The API returned a non-2xx status code.
    #[error("API error {status}: {body}")]
    Api {
        /// The HTTP status code.
        status: u16,
        /// The response body.
        body: String,
    },
    /// I/O error writing the `.error` file.
    #[error("IO error writing .error file: {0}")]
    Io(#[from] std::io::Error),
}

impl HarnessError {
    /// Build a descriptive `HarnessError::Http` from a `reqwest::Error`.
    ///
    /// Extracts timeout/connect status and walks the cause chain to produce a
    /// message that actually explains *why* the request failed, not just
    /// *that* it failed.
    fn from_reqwest_error(err: reqwest::Error, url: &str) -> Self {
        let mut parts: Vec<String> = Vec::new();

        // Timeout is the most actionable piece of information for operators —
        // it tells them to either raise the timeout or check if the server is
        // overloaded. Surface it prominently.
        if err.is_timeout() {
            parts.push("timeout reached".to_string());
        }

        // A connect error means the server was unreachable (connection
        // refused, DNS failure, etc.) — distinct from a timeout or a server
        // that returned an error status.
        if err.is_connect() {
            parts.push("connection failed".to_string());
        }

        // If we somehow got a status code back (e.g. from a redirect or the
        // response was received but the body read failed), include it.
        if let Some(status) = err.status() {
            parts.push(format!("status {}", status.as_u16()));
        }

        // Walk the cause chain to get the real root cause (e.g. "dns error:
        // failed to lookup address information", "connection refused", etc.).
        // reqwest::Error implements std::error::Error, so `source()` gives us
        // the next link in the chain.
        let mut chain_sources: Vec<String> = Vec::new();
        let mut source: Option<&dyn StdError> = err.source();
        while let Some(s) = source {
            let display = format!("{s}");
            // Avoid duplicate entries in the chain
            if !chain_sources.contains(&display) {
                chain_sources.push(display);
            }
            source = s.source();
        }

        // Build the final message: "error sending request for url (X):
        // [timeout reached] [connection failed] [status N]: cause1: cause2
        let mut message = format!("error sending request for url ({url})");

        if !parts.is_empty() {
            message.push_str(": ");
            message.push_str(&parts.join(", "));
        }

        if !chain_sources.is_empty() {
            message.push_str(": ");
            message.push_str(&chain_sources.join(": "));
        }

        HarnessError::Http { message }
    }
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
    /// Create a new `HermesClient`.
    ///
    /// `base_url` should be host-only **without trailing slash** (e.g. `http://localhost:8000`).
    /// The `/v1/responses` path is appended internally by `execute_step`.
    pub fn new(base_url: String, api_key: String) -> Self {
        HermesClient {
            base_url,
            api_key,
            client: Client::new(),
        }
    }

    /// Build the request body JSON for logging before the API call.
    ///
    /// Serializes a `HermesRequest` to pretty-printed JSON so
    /// the request data is available even if the API call fails.
    /// The returned string is the pretty-printed JSON that would be sent
    /// as the request body to the Hermes API.
    pub fn build_request_body(&self, instructions: Option<&str>, input: &str) -> String {
        let request = HermesRequest {
            instructions: instructions.map(|s| s.to_string()),
            input: input.to_string(),
            store: true,
        };
        serde_json::to_string_pretty(&request).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to pretty-print request, falling back to compact");
            serde_json::to_string(&request).unwrap_or_else(|_| "{}".to_string())
        })
    }

    /// Execute a single agent step.
    ///
    /// Convenience wrapper around `execute_step_with_error_path` with `None`
    /// for the error file path.
    pub async fn execute_step(
        &self,
        instructions: Option<&str>,
        input: &str,
    ) -> Result<StepResult, HarnessError> {
        self.execute_step_with_error_path(instructions, input, None)
            .await
    }

    /// Execute a single agent step with an explicit error file path.
    ///
    /// Returns a `StepResult` containing the extracted message, raw request
    /// body, and raw response body for audit logging.
    pub async fn execute_step_with_error_path(
        &self,
        instructions: Option<&str>,
        input: &str,
        error_path: Option<&Path>,
    ) -> Result<StepResult, HarnessError> {
        let url = format!("{}/v1/responses", self.base_url.trim_end_matches('/'));

        let request = HermesRequest {
            instructions: instructions.map(|s| s.to_string()),
            input: input.to_string(),
            store: true,
        };

        let raw_request = serde_json::to_string_pretty(&request).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Failed to pretty-print request, falling back to compact");
            serde_json::to_string(&request).unwrap_or_else(|_| "{}".to_string())
        });

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&request)
            .send()
            .await
            .map_err(|e| HarnessError::from_reqwest_error(e, &url))?;

        let status = response.status();
        let raw_response = response
            .text()
            .await
            .map_err(|e| HarnessError::from_reqwest_error(e, &url))?;

        if !status.is_success() {
            let error_path = error_path.unwrap_or(Path::new(".error"));
            let error_content = format!("status: {}\nbody: {}", status.as_u16(), raw_response);
            if let Err(e) = fs::write(error_path, &error_content) {
                return Err(HarnessError::Io(e));
            }
            return Err(HarnessError::Api {
                status: status.as_u16(),
                body: raw_response,
            });
        }

        let hermes_response: HermesResponse = serde_json::from_str(&raw_response).map_err(|e| {
            HarnessError::Api {
                status: 200,
                body: format!("Failed to parse response: {e}"),
            }
        })?;

        let extracted_message = hermes_response.extract_text();

        Ok(StepResult {
            extracted_message,
            raw_request,
            raw_response,
        })
    }
}

/// Response from the Hermes Agent `/health` endpoint.
///
/// The `/health` endpoint returns a JSON object with the agent's status,
/// platform identifier, and version. This struct is used by the startup
/// health check to verify that each configured agent is reachable and
/// reports a healthy status.
#[derive(Debug, Clone, Deserialize)]
pub struct HealthResponse {
    /// The agent's health status (e.g. `"ok"`).
    pub status: String,
    /// The platform identifier (e.g. `"hermes-agent"`).
    pub platform: String,
    /// The agent version string (e.g. `"0.17.0"`).
    pub version: String,
}

/// Errors that can occur during an agent health check.
#[derive(Debug, Error)]
pub enum HealthCheckError {
    /// HTTP request failed (network error, connection refused, timeout, etc.).
    #[error("Failed to connect to agent '{agent}' at {url}: {message}")]
    Http {
        /// The agent name from the configuration.
        agent: String,
        /// The health endpoint URL that was queried.
        url: String,
        /// Human-readable description of the failure.
        message: String,
    },
    /// The agent returned a non-200 status code.
    #[error("Agent '{agent}' at {url} returned status {status}: {body}")]
    BadStatus {
        /// The agent name from the configuration.
        agent: String,
        /// The health endpoint URL that was queried.
        url: String,
        /// The HTTP status code returned.
        status: u16,
        /// The response body.
        body: String,
    },
    /// The response body could not be parsed as a `HealthResponse`.
    #[error("Agent '{agent}' at {url} returned unparseable health response: {message}")]
    Parse {
        /// The agent name from the configuration.
        agent: String,
        /// The health endpoint URL that was queried.
        url: String,
        /// The parse error message.
        message: String,
    },
}

/// Check the health of a single agent by querying its `/health` endpoint.
///
/// Sends a GET request to `{base_url}/health` and verifies that the response
/// body contains a `HealthResponse` with `status: "ok"`.
///
/// Returns `Ok(HealthResponse)` if the agent is healthy, or an error
/// indicating the type of failure.
pub async fn check_agent_health(
    agent: &crate::config::AgentConfig,
) -> Result<HealthResponse, HealthCheckError> {
    let url = format!("{}/health", agent.base_url.as_str().trim_end_matches('/'));

    let response = reqwest::get(&url)
        .await
        .map_err(|e| HealthCheckError::Http {
            agent: agent.name.clone(),
            url: url.clone(),
            message: format!("{e}"),
        })?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| HealthCheckError::Http {
            agent: agent.name.clone(),
            url: url.clone(),
            message: format!("Failed to read response body: {e}"),
        })?;

    if !status.is_success() {
        return Err(HealthCheckError::BadStatus {
            agent: agent.name.clone(),
            url,
            status: status.as_u16(),
            body,
        });
    }

    let health: HealthResponse =
        serde_json::from_str(&body).map_err(|e| HealthCheckError::Parse {
            agent: agent.name.clone(),
            url: url.clone(),
            message: format!("{e}"),
        })?;

    if health.status != "ok" {
        return Err(HealthCheckError::BadStatus {
            agent: agent.name.clone(),
            url,
            status: 200,
            body: format!("status is '{}', expected 'ok'", health.status),
        });
    }

    Ok(health)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hermes_request_serialization() {
        let request = HermesRequest {
            instructions: Some("test instructions".to_string()),
            input: "test input".to_string(),
            store: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("test instructions"));
        assert!(json.contains("test input"));
        assert!(json.contains("true"));
    }

    #[test]
    fn test_hermes_request_instructions_omitted_when_none() {
        let request = HermesRequest {
            instructions: None,
            input: "test input".to_string(),
            store: true,
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(!json.contains("instructions"));
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
        let json = r#"{"type": "message", "role": "assistant", "content": []}"#;
        let item: OutputItem = serde_json::from_str(json).unwrap();
        assert_eq!(item.item_type, "message");
        assert_eq!(item.role, "assistant");
        assert!(item.content.is_empty());
    }

    #[test]
    fn test_hermes_response_parsing_with_nested_output() {
        let json = r#"{
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "Hello"},
                        {"type": "reasoning", "text": "thinking..."},
                        {"type": "output_text", "text": "World"}
                    ]
                }
            ]
        }"#;
        let response: HermesResponse = serde_json::from_str(json).unwrap();
        let text = response.extract_text();
        assert_eq!(text, "Hello\nWorld");
    }

    #[test]
    fn test_hermes_response_extracts_last_message() {
        let json = r#"{
            "output": [
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "first"}]},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "second"}]}
            ]
        }"#;
        let response: HermesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(response.extract_text(), "second");
    }

    #[test]
    fn test_hermes_response_no_message_output() {
        let json = r#"{"output": [{"type": "reasoning", "role": "", "content": []}]}"#;
        let response: HermesResponse = serde_json::from_str(json).unwrap();
        assert!(response.extract_text().is_empty());
    }

    #[test]
    fn test_hermes_response_empty_output() {
        let json = r#"{"output": []}"#;
        let response: HermesResponse = serde_json::from_str(json).unwrap();
        assert!(response.extract_text().is_empty());
    }

    #[test]
    fn test_step_result_fields() {
        let result = StepResult {
            extracted_message: "test".to_string(),
            raw_request: "request".to_string(),
            raw_response: "response".to_string(),
        };
        assert_eq!(result.extracted_message, "test");
        assert_eq!(result.raw_request, "request");
        assert_eq!(result.raw_response, "response");
    }

    #[test]
    fn test_hermes_client_new() {
        let client = HermesClient::new("http://localhost:8000".to_string(), "key".to_string());
        assert_eq!(client.base_url, "http://localhost:8000");
        assert_eq!(client.api_key, "key");
    }

    #[test]
    fn test_hermes_client_different_base_urls() {
        let client1 = HermesClient::new("http://localhost:8000".to_string(), "key1".to_string());
        let client2 = HermesClient::new("http://localhost:8001".to_string(), "key2".to_string());
        assert_ne!(client1.base_url, client2.base_url);
    }

    #[test]
    fn test_harness_error_api_display() {
        let err = HarnessError::Api {
            status: 500,
            body: "Internal Server Error".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("API error 500"));
        assert!(display.contains("Internal Server Error"));
    }

    #[test]
    fn test_harness_error_io_display() {
        let err = HarnessError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file not found",
        ));
        let display = format!("{err}");
        assert!(display.contains("IO error"));
        assert!(display.contains("file not found"));
    }

    #[test]
    fn test_harness_error_http_includes_url() {
        let err = HarnessError::Http {
            message: "error sending request for url (http://10.200.0.3:8500/v1/responses): timeout reached: operation timed out"
                .to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("HTTP request failed"));
        assert!(display.contains("http://10.200.0.3:8500/v1/responses"));
        assert!(display.contains("timeout reached"));
        assert!(display.contains("operation timed out"));
    }

    #[test]
    fn test_harness_error_http_connection_refused() {
        let err = HarnessError::Http {
            message: "error sending request for url (http://localhost:8500/v1/responses): connection failed: Connection refused (os error 111)"
                .to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("connection failed"));
        assert!(display.contains("Connection refused"));
    }

    #[test]
    fn test_harness_error_http_minimal() {
        // Even with no extra details, the message should still be meaningful.
        let err = HarnessError::Http {
            message: "error sending request for url (http://example.com/v1/responses)".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("HTTP request failed"));
        assert!(display.contains("http://example.com/v1/responses"));
    }

    #[test]
    fn test_health_check_error_http_display() {
        let err = HealthCheckError::Http {
            agent: "pm".to_string(),
            url: "http://localhost:8000/health".to_string(),
            message: "connection refused".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("Failed to connect to agent 'pm'"));
        assert!(display.contains("http://localhost:8000/health"));
        assert!(display.contains("connection refused"));
    }

    #[test]
    fn test_health_check_error_bad_status_display() {
        let err = HealthCheckError::BadStatus {
            agent: "swe".to_string(),
            url: "http://localhost:8001/health".to_string(),
            status: 503,
            body: "Service Unavailable".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("Agent 'swe'"));
        assert!(display.contains("http://localhost:8001/health"));
        assert!(display.contains("503"));
        assert!(display.contains("Service Unavailable"));
    }

    #[test]
    fn test_health_check_error_parse_display() {
        let err = HealthCheckError::Parse {
            agent: "reviewer".to_string(),
            url: "http://localhost:8002/health".to_string(),
            message: "expected value at line 1 column 1".to_string(),
        };
        let display = format!("{err}");
        assert!(display.contains("Agent 'reviewer'"));
        assert!(display.contains("http://localhost:8002/health"));
        assert!(display.contains("unparseable health response"));
        assert!(display.contains("expected value at line 1 column 1"));
    }
}
