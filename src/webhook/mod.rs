pub mod github;
pub mod gitlab;
pub mod gitlab_api;

use crate::config::Platform;
use crate::dispatcher::DispatchMessage;
use crate::workflow::TriggerType;
use tokio::sync::mpsc;
use tracing::instrument;

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
    /// Internal dispatcher error (HTTP 503).
    InternalError(String),
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            WebhookError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            WebhookError::NoMatchingTrigger(msg) => write!(f, "No matching trigger: {msg}"),
            WebhookError::InternalError(msg) => write!(f, "Internal error: {msg}"),
        }
    }
}

impl std::error::Error for WebhookError {}

/// Handler for webhook events that routes verified platform events
/// to a dispatcher via an mpsc channel.
#[derive(Clone)]
pub struct WebhookHandler {
    pub platform: Platform,
    pub secret: String,
    pub sender: mpsc::Sender<DispatchMessage>,
}

impl WebhookHandler {
    /// Create a new `WebhookHandler` with the given platform config, secret, and channel sender.
    pub fn new(platform: Platform, secret: String, sender: mpsc::Sender<DispatchMessage>) -> Self {
        Self {
            platform,
            secret,
            sender,
        }
    }

    /// Process a webhook request: verify, parse, and send the resulting
    /// `DispatchMessage` to the dispatcher channel.
    ///
    /// Returns `Ok(())` on success, or a `WebhookError` on failure.
    #[instrument(skip(self, body, token_or_signature), fields(platform = ?self.platform, event_type = %event_header))]
    pub async fn handle_webhook(
        &self,
        token_or_signature: &str,
        event_header: &str,
        body: &[u8],
    ) -> Result<(), WebhookError> {
        let trigger_event = dispatch_webhook(
            &self.platform,
            token_or_signature,
            event_header,
            body,
            &self.secret,
        )?;
        // Wrap TriggerEvent in DispatchMessage and send to dispatcher
        self.sender
            .send(DispatchMessage {
                event: trigger_event,
            })
            .await
            .map_err(|_| WebhookError::InternalError("Dispatcher channel full".to_string()))?;

        Ok(())
    }
}

/// Dispatch a webhook request to the platform-specific handler.
///
/// For GitLab (`platform = Gitlab`): verifies the `X-Gitlab-Token` header,
/// parses the JSON payload, and maps events to trigger types.
///
/// For GitHub (`platform = Github`): verifies the `X-Hub-Signature-256` HMAC
/// header, parses the event type, and maps events to trigger types.
///
/// Returns `Ok(TriggerEvent)` on success, or a `WebhookError` on failure.
#[instrument(skip(token_or_signature, body, secret), fields(platform = ?platform, event_type = %event_header))]
pub fn dispatch_webhook(
    platform: &Platform,
    token_or_signature: &str,
    event_header: &str,
    body: &[u8],
    secret: &str,
) -> Result<TriggerEvent, WebhookError> {
    let result = match platform {
        Platform::Gitlab => {
            gitlab::handle_gitlab_webhook(token_or_signature, event_header, body, secret)
        }
        Platform::Github => {
            github::handle_github_webhook(token_or_signature, event_header, body, secret)
        }
    };
    if let Ok(ref event) = result {
        tracing::Span::current().record("event_id", &event.event_id);
    }
    result
}
