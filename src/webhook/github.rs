use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::workflow::TriggerType;

use super::{TriggerEvent, WebhookError};

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;

/// Errors that can occur during GitHub webhook processing.
#[derive(Debug)]
#[allow(dead_code)]
pub enum GitHubWebhookError {
    /// The X-Hub-Signature-256 header is missing.
    MissingSignature,
    /// The signature format is invalid (e.g. missing "sha256=" prefix).
    InvalidSignatureFormat,
    /// The HMAC signature does not match the computed hash.
    SignatureMismatch,
    /// The X-GitHub-Event header is missing.
    MissingEventType,
    /// The event type is not one we handle.
    UnknownEventType(String),
    /// The JSON payload could not be parsed.
    PayloadParseError(String),
}

impl std::fmt::Display for GitHubWebhookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitHubWebhookError::MissingSignature => write!(f, "missing X-Hub-Signature-256 header"),
            GitHubWebhookError::InvalidSignatureFormat => {
                write!(f, "invalid signature format, expected sha256= prefix")
            }
            GitHubWebhookError::SignatureMismatch => {
                write!(f, "HMAC signature verification failed")
            }
            GitHubWebhookError::MissingEventType => write!(f, "missing X-GitHub-Event header"),
            GitHubWebhookError::UnknownEventType(t) => write!(f, "unknown event type: {t}"),
            GitHubWebhookError::PayloadParseError(msg) => {
                write!(f, "payload parse error: {msg}")
            }
        }
    }
}

impl std::error::Error for GitHubWebhookError {}

// ---------------------------------------------------------------------------
// GitHub webhook payload structs
// ---------------------------------------------------------------------------

/// Top-level GitHub webhook event, identified by the `X-GitHub-Event` header
/// and `action` field in the JSON payload.
#[derive(Debug, Clone)]
pub struct GitHubEvent {
    pub event_type: String,
    pub action: String,
    pub payload: GitHubPayload,
}

/// The parsed payload variants for known GitHub event types.
#[derive(Debug, Clone)]
pub enum GitHubPayload {
    Issues(IssuesPayload),
    IssueComment(IssueCommentPayload),
    PullRequestReview(PullRequestReviewPayload),
    PullRequestReviewComment(PullRequestReviewCommentPayload),
}

/// Payload for GitHub `issues` events.
#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct IssuesPayload {
    pub action: String,
    pub issue: IssueDetails,
    pub sender: SenderDetails,
}

/// Payload for GitHub `issue_comment` events.
#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct IssueCommentPayload {
    pub action: String,
    pub comment: CommentDetails,
    pub issue: IssueDetails,
    pub sender: SenderDetails,
}

/// Payload for GitHub `pull_request_review` events.
#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct PullRequestReviewPayload {
    pub action: String,
    pub review: ReviewDetails,
    pub pull_request: PullRequestDetails,
    pub sender: SenderDetails,
}

/// Payload for GitHub `pull_request_review_comment` events.
#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct PullRequestReviewCommentPayload {
    pub action: String,
    pub comment: ReviewCommentDetails,
    pub pull_request: PullRequestDetails,
    pub sender: SenderDetails,
}

// ---------------------------------------------------------------------------
// Nested data structs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct IssueDetails {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub assignee: Option<UserDetails>,
    #[serde(default)]
    pub assignees: Vec<UserDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct UserDetails {
    pub login: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct SenderDetails {
    pub login: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct CommentDetails {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct ReviewDetails {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    pub user: Option<UserDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
pub struct ReviewCommentDetails {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub pull_request_review_id: Option<u64>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PullRequestDetails {
    pub number: u64,
}

// ---------------------------------------------------------------------------
// HMAC-SHA256 signature verification
// ---------------------------------------------------------------------------

/// Verify that the `X-Hub-Signature-256` header value matches the HMAC-SHA256
/// of the request body computed with the given secret.
///
/// The signature header must be in the format `sha256=<hex>`. Uses
/// constant-time comparison to prevent timing attacks.
pub fn verify_github_signature(
    payload: &[u8],
    signature: &str,
    secret: &str,
) -> Result<(), GitHubWebhookError> {
    let signature_hex = signature
        .strip_prefix("sha256=")
        .ok_or(GitHubWebhookError::InvalidSignatureFormat)?;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(payload);
    let result = mac.finalize().into_bytes();

    let computed_hex = hex::encode(result);

    // Constant-time comparison to prevent timing attacks.
    // Both strings are guaranteed to be 64 chars (SHA-256 hex output).
    if computed_hex
        .as_bytes()
        .ct_eq(signature_hex.as_bytes())
        .unwrap_u8()
        == 1
    {
        Ok(())
    } else {
        Err(GitHubWebhookError::SignatureMismatch)
    }
}

// ---------------------------------------------------------------------------
// Event parsing
// ---------------------------------------------------------------------------

/// Parse a raw JSON payload into a `GitHubEvent`, using the `X-GitHub-Event`
/// header value to determine which struct to deserialize into.
pub fn parse_github_event(
    event_header: &str,
    body: &[u8],
) -> Result<GitHubEvent, GitHubWebhookError> {
    match event_header {
        "issues" => {
            let payload: IssuesPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            Ok(GitHubEvent {
                event_type: "issues".to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::Issues(payload),
            })
        }
        "issue_comment" => {
            let payload: IssueCommentPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            Ok(GitHubEvent {
                event_type: "issue_comment".to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::IssueComment(payload),
            })
        }
        "pull_request_review" => {
            let payload: PullRequestReviewPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            Ok(GitHubEvent {
                event_type: "pull_request_review".to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::PullRequestReview(payload),
            })
        }
        "pull_request_review_comment" => {
            let payload: PullRequestReviewCommentPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            Ok(GitHubEvent {
                event_type: "pull_request_review_comment".to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::PullRequestReviewComment(payload),
            })
        }
        _ => Err(GitHubWebhookError::UnknownEventType(
            event_header.to_string(),
        )),
    }
}

// ---------------------------------------------------------------------------
// Trigger mapping
// ---------------------------------------------------------------------------

/// Map a parsed `GitHubEvent` to a `TriggerType`.
///
/// Returns `None` if the event action doesn't match any configured trigger
/// type (e.g. an `issues` event with action `opened` instead of `assigned`).
pub fn map_to_trigger_event(event: &GitHubEvent) -> Option<TriggerType> {
    match (&event.event_type as &str, event.action.as_str()) {
        ("issues", "assigned") => {
            let payload = match &event.payload {
                GitHubPayload::Issues(p) => p,
                _ => return None,
            };
            Some(TriggerType::GithubIssueAssigned {
                assigned_to: payload.issue.assignee.as_ref().map(|a| a.login.clone()),
                allowed_users: None,
            })
        }
        ("issue_comment", "created") => {
            let _payload = match &event.payload {
                GitHubPayload::IssueComment(p) => p,
                _ => return None,
            };
            // issue_comment covers both issue and PR comments in GitHub's API;
            // let the dispatcher handle issue-vs-PR filtering at a higher layer.
            Some(TriggerType::GithubIssueCommentMention {
                mentioned_user: None,
                allowed_users: None,
            })
        }
        ("pull_request_review", "submitted") => {
            let _payload = match &event.payload {
                GitHubPayload::PullRequestReview(p) => p,
                _ => return None,
            };
            Some(TriggerType::GithubPullRequestReview {
                allowed_users: None,
            })
        }
        ("pull_request_review_comment", "created") => {
            let payload = match &event.payload {
                GitHubPayload::PullRequestReviewComment(p) => p,
                _ => return None,
            };
            let _review_id = payload
                .comment
                .pull_request_review_id
                .unwrap_or(payload.comment.id);
            Some(TriggerType::GithubPullRequestCommentMention {
                mentioned_user: None,
                allowed_users: None,
            })
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Webhook handler
// ---------------------------------------------------------------------------

/// Handle a GitHub webhook request.
///
/// Verifies the HMAC-SHA256 signature, parses the event payload, and maps it
/// to a trigger type. Returns `Ok(TriggerEvent)` on success, or a
/// `WebhookError` on failure.
pub fn handle_github_webhook(
    signature: &str,
    _event_header: &str,
    body: &[u8],
    secret: &str,
) -> Result<TriggerEvent, WebhookError> {
    // Step 1: Verify HMAC signature
    verify_github_signature(body, signature, secret)
        .map_err(|e| WebhookError::Unauthorized(e.to_string()))?;

    // Step 2: Extract event type from X-GitHub-Event header
    // Note: event_header is passed through but GitHub uses content-based parsing
    // The event type is determined by the payload's action field combined with event_header
    // We need the event_header to determine which struct to parse into.
    // However, the dispatch function passes the event_header; we'll use it for parsing.

    // Step 3: Parse the event payload
    // We need the event_header to determine which payload struct to use.
    // The dispatch function passes it but we ignore it here since we parse
    // directly from the body. For now, we need to know the event type.
    // Let's re-approach: the caller should provide the event type header.
    // Actually, looking at the original github.rs, it takes event_type as a param.
    // But dispatch_webhook passes event_header. We need to use it here.

    // The issue is that handle_github_webhook needs the event_header to parse the payload.
    // Let's use it properly.
    let event = parse_github_event(_event_header, body).map_err(|e| match e {
        GitHubWebhookError::UnknownEventType(t) => {
            // Unknown event types are a no-op — return 200 so the platform doesn't retry
            WebhookError::NoMatchingTrigger(format!("unhandled event type: {t}"))
        }
        _ => WebhookError::BadRequest(e.to_string()),
    })?;

    // Step 4: Map to a trigger type
    let trigger_type = map_to_trigger_event(&event).ok_or_else(|| {
        WebhookError::NoMatchingTrigger(format!(
            "no matching trigger for event '{}' action '{}'",
            event.event_type, event.action
        ))
    })?;

    // Step 5: Build result
    // Note: GitHub webhook payloads don't include repo info in a simple format.
    // The repo info would come from the webhook URL path or repository object.
    // For now, use placeholder values — this will be wired up when dispatcher is added.
    let repo_path = String::new();
    let event_id = match &event.payload {
        GitHubPayload::Issues(p) => format!("issue-{}", p.issue.number),
        GitHubPayload::IssueComment(p) => {
            format!("issue-{}-comment-{}", p.issue.number, p.comment.id)
        }
        GitHubPayload::PullRequestReview(p) => {
            format!("pr-{}-review-{}", p.pull_request.number, p.review.id)
        }
        GitHubPayload::PullRequestReviewComment(p) => {
            format!("pr-{}-comment-{}", p.pull_request.number, p.comment.id)
        }
    };

    Ok(TriggerEvent {
        trigger_type,
        repo_path,
        event_id,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- HMAC verification tests ---

    #[test]
    fn test_verify_valid_signature() {
        let payload = b"test payload data";
        let secret = "my-webhook-secret";

        // Compute the expected HMAC-SHA256
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        let expected_hex = hex::encode(result);
        let signature = format!("sha256={expected_hex}");

        assert!(verify_github_signature(payload, &signature, secret).is_ok());
    }

    #[test]
    fn test_verify_invalid_signature() {
        let payload = b"test payload data";
        let secret = "my-webhook-secret";
        let wrong_signature =
            "sha256=0000000000000000000000000000000000000000000000000000000000000000";

        let result = verify_github_signature(payload, wrong_signature, secret);
        assert!(matches!(result, Err(GitHubWebhookError::SignatureMismatch)));
    }

    #[test]
    fn test_verify_missing_sha256_prefix() {
        let payload = b"test payload data";
        let secret = "my-webhook-secret";
        let signature_without_prefix =
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

        let result = verify_github_signature(payload, signature_without_prefix, secret);
        assert!(matches!(
            result,
            Err(GitHubWebhookError::InvalidSignatureFormat)
        ));
    }

    #[test]
    fn test_verify_wrong_secret() {
        let payload = b"test payload data";
        let correct_secret = "correct-secret";
        let wrong_secret = "wrong-secret";

        // Compute signature with correct secret
        let mut mac = HmacSha256::new_from_slice(correct_secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        let expected_hex = hex::encode(result);
        let signature = format!("sha256={expected_hex}");

        // Verify with wrong secret should fail
        let result = verify_github_signature(payload, &signature, wrong_secret);
        assert!(matches!(result, Err(GitHubWebhookError::SignatureMismatch)));
    }

    // --- Event parsing tests ---

    #[test]
    fn test_parse_issues_event() {
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

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "issues");
        assert_eq!(event.action, "assigned");

        if let GitHubPayload::Issues(payload) = &event.payload {
            assert_eq!(payload.issue.number, 42);
            assert_eq!(payload.issue.title, "Bug report");
            assert_eq!(payload.issue.assignee.as_ref().unwrap().login, "alice");
            assert_eq!(payload.sender.login, "bob");
        } else {
            panic!("Expected IssuesPayload");
        }
    }

    #[test]
    fn test_parse_issue_comment_event() {
        let body = r#"{
            "action": "created",
            "comment": {
                "id": 12345,
                "body": "@alice please review this"
            },
            "issue": {
                "number": 42,
                "title": "Some issue",
                "assignees": []
            },
            "sender": {"login": "charlie"}
        }"#;

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "issue_comment");
        assert_eq!(event.action, "created");

        if let GitHubPayload::IssueComment(payload) = &event.payload {
            assert_eq!(payload.comment.id, 12345);
            assert_eq!(payload.issue.number, 42);
        } else {
            panic!("Expected IssueCommentPayload");
        }
    }

    #[test]
    fn test_parse_pull_request_review_event() {
        let body = r#"{
            "action": "submitted",
            "review": {
                "id": 999,
                "body": "Looks good to me",
                "user": {"login": "reviewer"}
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"}
        }"#;

        let event = parse_github_event("pull_request_review", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "pull_request_review");
        assert_eq!(event.action, "submitted");

        if let GitHubPayload::PullRequestReview(payload) = &event.payload {
            assert_eq!(payload.review.id, 999);
            assert_eq!(payload.pull_request.number, 7);
        } else {
            panic!("Expected PullRequestReviewPayload");
        }
    }

    #[test]
    fn test_parse_pull_request_review_comment_event() {
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

        let event = parse_github_event("pull_request_review_comment", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "pull_request_review_comment");
        assert_eq!(event.action, "created");

        if let GitHubPayload::PullRequestReviewComment(payload) = &event.payload {
            assert_eq!(payload.comment.id, 555);
            assert_eq!(payload.comment.pull_request_review_id, Some(999));
            assert_eq!(payload.pull_request.number, 7);
        } else {
            panic!("Expected PullRequestReviewCommentPayload");
        }
    }

    #[test]
    fn test_parse_unknown_event_type() {
        let result = parse_github_event("push", b"{}");
        assert!(matches!(
            result,
            Err(GitHubWebhookError::UnknownEventType(ref t)) if t == "push"
        ));
    }

    #[test]
    fn test_parse_invalid_json() {
        let result = parse_github_event("issues", b"not json");
        assert!(matches!(
            result,
            Err(GitHubWebhookError::PayloadParseError(_))
        ));
    }

    // --- Trigger mapping tests ---

    #[test]
    fn test_map_issues_assigned() {
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

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(trigger, TriggerType::GithubIssueAssigned { .. }));
        assert_eq!(trigger.label(), "github_issue_assigned");
    }

    #[test]
    fn test_map_issue_comment_created() {
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

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(
            trigger,
            TriggerType::GithubIssueCommentMention { .. }
        ));
        assert_eq!(trigger.label(), "github_issue_comment_mention");
    }

    #[test]
    fn test_map_pull_request_review_submitted() {
        let body = r#"{
            "action": "submitted",
            "review": {
                "id": 999,
                "body": "LGTM",
                "user": {"login": "reviewer"}
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"}
        }"#;

        let event = parse_github_event("pull_request_review", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(
            trigger,
            TriggerType::GithubPullRequestReview { .. }
        ));
        assert_eq!(trigger.label(), "github_pull_request_review");
    }

    #[test]
    fn test_map_pull_request_review_comment_created() {
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

        let event = parse_github_event("pull_request_review_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(
            trigger,
            TriggerType::GithubPullRequestCommentMention { .. }
        ));
        assert_eq!(trigger.label(), "github_pull_request_review_comment");
    }

    #[test]
    fn test_map_unknown_action_returns_none() {
        let body = r#"{
            "action": "opened",
            "issue": {
                "number": 42,
                "title": "Bug report",
                "assignees": []
            },
            "sender": {"login": "bob"}
        }"#;

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        let result = map_to_trigger_event(&event);
        assert!(result.is_none());
    }

    #[test]
    fn test_map_unsupported_event_type_returns_err() {
        // "push" events don't map to any trigger
        let result = parse_github_event("push", b"{}");
        assert!(result.is_err());
    }
}
