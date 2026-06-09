pub mod github;
pub mod github_api;
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
    /// The username of the user who performed the action that triggered
    /// this event (the "actor"). Extracted from the webhook payload's
    /// `sender.login` (GitHub) or `user.username` (GitLab).
    /// Used by the dispatcher to authorize the event against the workflow's
    /// `allowed_users` list.
    pub actor: String,
    /// Trigger-specific template variables extracted from the webhook payload.
    /// These are merged with global variables in the dispatcher before
    /// being passed to the workflow runner for template rendering.
    pub variables: std::collections::HashMap<String, String>,
    /// Platform-specific delivery ID for watermark tracking.
    /// GitHub: the `X-GitHub-Delivery` header UUID.
    /// GitLab: not currently extracted (reserved for future use).
    pub delivery_id: Option<String>,
    /// The source branch name for PR/MR review events.
    /// Populated for `github_pull_request_review`, `github_pull_request_comment_mention`,
    /// and `gitlab_merge_request_review` / `gitlab_merge_request_comment_mention` events.
    /// Used by the dispatcher to create an isolated worktree at the correct branch.
    /// `None` for non-review events (issue assignments, issue comments).
    pub branch: Option<String>,
}

/// Errors that can occur during webhook processing.
#[derive(Debug)]
pub enum WebhookError {
    /// Token verification failed (HTTP 401).
    Unauthorized(String),
    /// Payload could not be parsed (HTTP 400).
    BadRequest(String),
    /// Event type not supported or no matching trigger (HTTP 200, no-op).
    /// Carries the event type and action as structured fields so callers
    /// can log them as separate tracing parameters rather than a single
    /// formatted string.
    NoMatchingTrigger {
        /// The platform-specific event type (e.g. "issues", "Issue Hook").
        event: String,
        /// The action within that event (e.g. "edited", "update").
        action: String,
    },
    /// Internal dispatcher error (HTTP 503).
    InternalError(String),
}

impl std::fmt::Display for WebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WebhookError::Unauthorized(msg) => write!(f, "Unauthorized: {msg}"),
            WebhookError::BadRequest(msg) => write!(f, "Bad request: {msg}"),
            WebhookError::NoMatchingTrigger { event, action } => {
                write!(f, "No matching trigger: event='{event}' action='{action}'")
            }
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
        delivery_id: Option<String>,
    ) -> Result<(), WebhookError> {
        let mut trigger_event = dispatch_webhook(
            &self.platform,
            token_or_signature,
            event_header,
            body,
            &self.secret,
        )?;
        trigger_event.delivery_id = delivery_id;
        // Wrap TriggerEvent in DispatchMessage and send to dispatcher
        let event_id = trigger_event.event_id.clone();
        let trigger_type = format!("{:?}", trigger_event.trigger_type);
        self.sender
            .send(DispatchMessage {
                event: trigger_event,
            })
            .await
            .map_err(|_| WebhookError::InternalError("Dispatcher channel closed".to_string()))?;
        tracing::info!(event_id = %event_id, trigger_type = %trigger_type, "Webhook event dispatched to dispatcher");

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
