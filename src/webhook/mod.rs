pub mod github;
pub mod gitlab;

use crate::config::Platform;
use crate::workflow::TriggerType;

/// Internal representation of a parsed and verified webhook event,
/// ready to be sent to the dispatcher channel.
#[derive(Debug, Clone)]
pub struct TriggerEvent {
    /// Which trigger type matched.
    pub trigger_type: TriggerType,
    /// The owner/repository path (e.g. "internal-team/backend-service").
    pub repo_path: String,
    /// A unique event ID for deduplication (e.g. "issue-42").
    pub event_id: String,
}

/// Errors that can occur during webhook processing.
#[derive(Debug)]
pub enum WebhookError {
    /// Token verification failed (HTTP 401).
    Unauthorized(String),
    /// Payload could not be parsed (HTTP 400).
    BadRequest(String),
    /// Event type not supported or no matching trigger (HTTP 200, no-op).
    NoMatchingTrigger(String),
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            WebhookError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            WebhookError::NoMatchingTrigger(msg) => write!(f, "No matching trigger: {msg}"),
        }
    }
}

impl std::error::Error for WebhookError {}

/// Dispatch a webhook request to the platform-specific handler.
///
/// For GitLab (`platform = Gitlab`): verifies the `X-Gitlab-Token` header,
/// parses the JSON payload, and maps events to trigger types.
///
/// For GitHub (`platform = Github`): verifies the `X-Hub-Signature-256` HMAC
/// header, parses the event type, and maps events to trigger types.
///
/// Returns `Ok(TriggerEvent)` on success, or a `WebhookError` on failure.
pub fn dispatch_webhook(
    platform: &Platform,
    token_or_signature: &str,
    event_header: &str,
    body: &[u8],
    secret: &str,
) -> Result<TriggerEvent, WebhookError> {
    match platform {
        Platform::Gitlab => {
            gitlab::handle_gitlab_webhook(token_or_signature, event_header, body, secret)
        }
        Platform::Github => {
            github::handle_github_webhook(token_or_signature, event_header, body, secret)
        }
    }
}
