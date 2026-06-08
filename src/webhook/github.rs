use hmac::{Hmac, Mac};
use sha2::Sha256;
use subtle::ConstantTimeEq;

use crate::workflow::TriggerType;

use super::{TriggerEvent, WebhookError};

/// GitHub webhook event type strings (X-GitHub-Event header values).
pub const GITHUB_PUSH: &str = "push";
/// GitHub pull request event type.
pub const GITHUB_PULL_REQUEST: &str = "pull_request";
/// GitHub issues event type.
pub const GITHUB_ISSUES: &str = "issues";
/// GitHub issue comment event type.
pub const GITHUB_ISSUE_COMMENT: &str = "issue_comment";
/// GitHub pull request review event type.
pub const GITHUB_PULL_REQUEST_REVIEW: &str = "pull_request_review";
/// GitHub pull request review comment event type.
pub const GITHUB_PULL_REQUEST_REVIEW_COMMENT: &str = "pull_request_review_comment";

/// HMAC-SHA256 type alias.
type HmacSha256 = Hmac<Sha256>;

/// Errors that can occur during GitHub webhook processing.
#[derive(Debug)]
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
    pub repository: RepositoryDetails,
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
pub struct IssuesPayload {
    pub action: String,
    pub issue: IssueDetails,
    pub sender: SenderDetails,
    pub repository: RepositoryDetails,
}

/// Payload for GitHub `issue_comment` events.
///
/// GitHub uses `issue_comment` for both issue and PR comments. When the
/// comment is on a PR, the payload includes a `pull_request` field.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct IssueCommentPayload {
    pub action: String,
    pub comment: CommentDetails,
    pub issue: IssueDetails,
    pub sender: SenderDetails,
    pub repository: RepositoryDetails,
    #[serde(default)]
    pub pull_request: Option<PullRequestDetails>,
}

/// Payload for GitHub `pull_request_review` events.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PullRequestReviewPayload {
    pub action: String,
    pub review: ReviewDetails,
    pub pull_request: PullRequestDetails,
    pub sender: SenderDetails,
    pub repository: RepositoryDetails,
}

/// Payload for GitHub `pull_request_review_comment` events.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct PullRequestReviewCommentPayload {
    pub action: String,
    pub comment: ReviewCommentDetails,
    pub pull_request: PullRequestDetails,
    pub sender: SenderDetails,
    pub repository: RepositoryDetails,
}

// ---------------------------------------------------------------------------
// Nested data structs
// ---------------------------------------------------------------------------

/// Repository details extracted from the top-level `repository` field in
/// GitHub webhook payloads. Used to derive `repo_path` as `owner/name`.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct RepositoryDetails {
    pub full_name: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
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
pub struct SenderDetails {
    pub login: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct CommentDetails {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct ReviewDetails {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    pub user: Option<UserDetails>,
}

#[derive(Debug, Clone, serde::Deserialize)]
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
        GITHUB_ISSUES => {
            let payload: IssuesPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            let repo_path = payload.repository.full_name.clone();
            Ok(GitHubEvent {
                event_type: GITHUB_ISSUES.to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::Issues(payload),
                repository: RepositoryDetails {
                    full_name: repo_path,
                },
            })
        }
        GITHUB_ISSUE_COMMENT => {
            let payload: IssueCommentPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            let repo_path = payload.repository.full_name.clone();
            Ok(GitHubEvent {
                event_type: GITHUB_ISSUE_COMMENT.to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::IssueComment(payload),
                repository: RepositoryDetails {
                    full_name: repo_path,
                },
            })
        }
        GITHUB_PULL_REQUEST_REVIEW => {
            let payload: PullRequestReviewPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            let repo_path = payload.repository.full_name.clone();
            Ok(GitHubEvent {
                event_type: GITHUB_PULL_REQUEST_REVIEW.to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::PullRequestReview(payload),
                repository: RepositoryDetails {
                    full_name: repo_path,
                },
            })
        }
        GITHUB_PULL_REQUEST_REVIEW_COMMENT => {
            let payload: PullRequestReviewCommentPayload = serde_json::from_slice(body)
                .map_err(|e| GitHubWebhookError::PayloadParseError(e.to_string()))?;
            let repo_path = payload.repository.full_name.clone();
            Ok(GitHubEvent {
                event_type: GITHUB_PULL_REQUEST_REVIEW_COMMENT.to_string(),
                action: payload.action.clone(),
                payload: GitHubPayload::PullRequestReviewComment(payload),
                repository: RepositoryDetails {
                    full_name: repo_path,
                },
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
        (GITHUB_ISSUES, "assigned") => {
            let payload = match &event.payload {
                GitHubPayload::Issues(p) => p,
                _ => return None,
            };
            Some(TriggerType::GithubIssueAssigned {
                assigned_to: payload.issue.assignee.as_ref().map(|a| a.login.clone()),
            })
        }
        (GITHUB_ISSUE_COMMENT, "created") => {
            let payload = match &event.payload {
                GitHubPayload::IssueComment(p) => p,
                _ => return None,
            };
            // GitHub uses issue_comment for both issue and PR comments.
            // When a pull_request field is present, this is a comment on a PR,
            // not a genuine issue comment — map it to the PR comment trigger.
            if payload.pull_request.is_some() {
                Some(TriggerType::GithubPullRequestCommentMention {
                    mentioned_user: None,
                })
            } else {
                Some(TriggerType::GithubIssueCommentMention {
                    mentioned_user: None,
                })
            }
        }
        (GITHUB_PULL_REQUEST_REVIEW, "submitted") => {
            let _payload = match &event.payload {
                GitHubPayload::PullRequestReview(p) => p,
                _ => return None,
            };
            Some(TriggerType::GithubPullRequestReview)
        }
        (GITHUB_PULL_REQUEST_REVIEW_COMMENT, "created") => {
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
            WebhookError::NoMatchingTrigger {
                event: t,
                action: String::new(),
            }
        }
        _ => WebhookError::BadRequest(e.to_string()),
    })?;

    // Step 4: Map to a trigger type
    let trigger_type =
        map_to_trigger_event(&event).ok_or_else(|| WebhookError::NoMatchingTrigger {
            event: event.event_type.clone(),
            action: event.action.clone(),
        })?;

    // Step 5: Build result with trigger-specific variables and repo_path
    let repo_path = event.repository.full_name.clone();
    let event_id = match &event.payload {
        GitHubPayload::Issues(p) => format!("issue-{}", p.issue.number),
        GitHubPayload::IssueComment(p) => {
            // When the comment is on a PR, use PR-style event_id format
            if let Some(pr) = &p.pull_request {
                format!("pr-{}-comment-{}", pr.number, p.comment.id)
            } else {
                format!("issue-{}-comment-{}", p.issue.number, p.comment.id)
            }
        }
        GitHubPayload::PullRequestReview(p) => {
            format!("pr-{}-review-{}", p.pull_request.number, p.review.id)
        }
        GitHubPayload::PullRequestReviewComment(p) => {
            format!("pr-{}-comment-{}", p.pull_request.number, p.comment.id)
        }
    };

    // Extract trigger-specific variables from the payload
    let mut variables = std::collections::HashMap::new();
    match &event.payload {
        GitHubPayload::Issues(p) => {
            variables.insert("issue_number".to_string(), p.issue.number.to_string());
            variables.insert(
                "assignee".to_string(),
                p.issue
                    .assignee
                    .as_ref()
                    .map(|a| a.login.clone())
                    .unwrap_or_default(),
            );
            variables.insert("issue_title".to_string(), p.issue.title.clone());
            variables.insert(
                "issue_body".to_string(),
                p.issue.body.clone().unwrap_or_default(),
            );
        }
        GitHubPayload::IssueComment(p) => {
            if let Some(pr) = &p.pull_request {
                // Comment on a PR via issue_comment event — use PR variables
                variables.insert("pr_number".to_string(), pr.number.to_string());
                variables.insert("comment_id".to_string(), p.comment.id.to_string());
                variables.insert(
                    "comment_body".to_string(),
                    p.comment.body.clone().unwrap_or_default(),
                );
            } else {
                // Genuine issue comment
                variables.insert("issue_number".to_string(), p.issue.number.to_string());
                variables.insert("comment_id".to_string(), p.comment.id.to_string());
                variables.insert(
                    "comment_body".to_string(),
                    p.comment.body.clone().unwrap_or_default(),
                );
            }
        }
        GitHubPayload::PullRequestReview(p) => {
            variables.insert("pr_number".to_string(), p.pull_request.number.to_string());
            variables.insert("review_id".to_string(), p.review.id.to_string());
            variables.insert(
                "review_body".to_string(),
                p.review.body.clone().unwrap_or_default(),
            );
        }
        GitHubPayload::PullRequestReviewComment(p) => {
            variables.insert("pr_number".to_string(), p.pull_request.number.to_string());
            variables.insert(
                "review_id".to_string(),
                p.comment
                    .pull_request_review_id
                    .unwrap_or(p.comment.id)
                    .to_string(),
            );
            variables.insert("comment_id".to_string(), p.comment.id.to_string());
            variables.insert(
                "comment_body".to_string(),
                p.comment.body.clone().unwrap_or_default(),
            );
        }
    }

    // Extract the actor (sender) from the webhook payload.
    // Per the architecture design, the actor is the user who performed the
    // action that created the webhook event (e.g. the person who assigned the
    // issue, wrote the comment, or submitted the review). This is used to
    // authorize the event against the workflow's `allowed_users` list.
    let actor = match &event.payload {
        GitHubPayload::Issues(p) => p.sender.login.clone(),
        GitHubPayload::IssueComment(p) => p.sender.login.clone(),
        GitHubPayload::PullRequestReview(p) => p
            .review
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_else(|| p.sender.login.clone()),
        GitHubPayload::PullRequestReviewComment(p) => p.sender.login.clone(),
    };

    Ok(TriggerEvent {
        trigger_type,
        repo_path,
        event_id,
        actor,
        variables,
        delivery_id: None,
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
            "sender": {"login": "bob"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "issues");
        assert_eq!(event.action, "assigned");
        assert_eq!(event.repository.full_name, "owner/repo");

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
            "sender": {"login": "charlie"},
            "repository": {"full_name": "owner/repo"}
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
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
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
            "sender": {"login": "commenter"},
            "repository": {"full_name": "owner/repo"}
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
        let result = parse_github_event(GITHUB_PUSH, b"{}");
        assert!(matches!(
            result,
            Err(GitHubWebhookError::UnknownEventType(ref t)) if t == GITHUB_PUSH
        ));
    }

    #[test]
    fn test_parse_invalid_json() {
        let result = parse_github_event(GITHUB_ISSUES, b"not json");
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
            "sender": {"login": "bob"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(trigger, TriggerType::GithubIssueAssigned { .. }));
        assert_eq!(
            trigger.label(),
            crate::workflow::triggers::GITHUB_ISSUE_ASSIGNED
        );
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
            "sender": {"login": "charlie"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(
            trigger,
            TriggerType::GithubIssueCommentMention { .. }
        ));
        assert_eq!(
            trigger.label(),
            crate::workflow::triggers::GITHUB_ISSUE_COMMENT_MENTION
        );
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
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("pull_request_review", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(trigger, TriggerType::GithubPullRequestReview));
        assert_eq!(
            trigger.label(),
            crate::workflow::triggers::GITHUB_PULL_REQUEST_REVIEW
        );
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
            "sender": {"login": "commenter"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("pull_request_review_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).unwrap();

        assert!(matches!(
            trigger,
            TriggerType::GithubPullRequestCommentMention { .. }
        ));
        assert_eq!(
            trigger.label(),
            crate::workflow::triggers::GITHUB_PULL_REQUEST_COMMENT_MENTION
        );
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
            "sender": {"login": "bob"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issues", body.as_bytes()).unwrap();
        let result = map_to_trigger_event(&event);
        assert!(result.is_none());
    }

    #[test]
    fn test_map_unsupported_event_type_returns_err() {
        // "push" events don't map to any trigger
        let result = parse_github_event(GITHUB_PUSH, b"{}");
        assert!(result.is_err());
    }

    // --- Additional HMAC verification tests ---

    #[test]
    fn test_verify_empty_signature() {
        // An empty string is not a valid signature (no "sha256=" prefix)
        let payload = b"test payload data";
        let secret = "my-webhook-secret";
        let result = verify_github_signature(payload, "", secret);
        assert!(matches!(
            result,
            Err(GitHubWebhookError::InvalidSignatureFormat)
        ));
    }

    #[test]
    fn test_verify_empty_payload_with_valid_signature() {
        // Empty payload should still verify correctly with the right HMAC
        let payload = b"";
        let secret = "secret";
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        let expected_hex = hex::encode(result);
        let signature = format!("sha256={expected_hex}");

        assert!(verify_github_signature(payload, &signature, secret).is_ok());
    }

    // --- Integration tests for handle_github_webhook ---

    fn make_signature(payload: &[u8], secret: &str) -> String {
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let result = mac.finalize().into_bytes();
        format!("sha256={}", hex::encode(result))
    }

    #[test]
    fn test_handle_github_webhook_full_pipeline_issues_assigned() {
        let secret = "test-secret";
        let body = r#"{
            "action": "assigned",
            "issue": {
                "number": 42,
                "title": "Bug",
                "body": "desc",
                "assignee": {"login": "alice"},
                "assignees": [{"login": "alice"}]
            },
            "sender": {"login": "bob"},
            "repository": {"full_name": "mintybasil/yoke"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_ISSUES, payload, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GithubIssueAssigned { .. }
        ));
        assert_eq!(event.event_id, "issue-42");
        assert_eq!(event.repo_path, "mintybasil/yoke");
        assert_eq!(event.variables.get("issue_number").unwrap(), "42");
        assert_eq!(event.variables.get("issue_title").unwrap(), "Bug");
    }

    #[test]
    fn test_handle_github_webhook_full_pipeline_issue_comment() {
        let secret = "test-secret";
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
            "sender": {"login": "charlie"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_ISSUE_COMMENT, payload, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GithubIssueCommentMention { .. }
        ));
        assert_eq!(event.event_id, "issue-42-comment-12345");
        assert_eq!(event.variables.get("comment_id").unwrap(), "12345");
    }

    #[test]
    fn test_handle_github_webhook_full_pipeline_pr_review() {
        let secret = "test-secret";
        let body = r#"{
            "action": "submitted",
            "review": {
                "id": 999,
                "body": "LGTM",
                "user": {"login": "reviewer"}
            },
            "pull_request": {"number": 7},
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_PULL_REQUEST_REVIEW, payload, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GithubPullRequestReview
        ));
        assert_eq!(event.event_id, "pr-7-review-999");
        assert_eq!(event.variables.get("pr_number").unwrap(), "7");
        assert_eq!(event.variables.get("review_id").unwrap(), "999");
    }

    #[test]
    fn test_handle_github_webhook_full_pipeline_pr_review_comment() {
        let secret = "test-secret";
        let body = r#"{
            "action": "created",
            "comment": {
                "id": 555,
                "body": "Nit: fix typo",
                "pull_request_review_id": 999
            },
            "pull_request": {"number": 7},
            "sender": {"login": "commenter"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(
            &signature,
            GITHUB_PULL_REQUEST_REVIEW_COMMENT,
            payload,
            secret,
        );
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GithubPullRequestCommentMention { .. }
        ));
        assert_eq!(event.event_id, "pr-7-comment-555");
        assert_eq!(event.variables.get("comment_id").unwrap(), "555");
    }

    #[test]
    fn test_handle_github_webhook_invalid_signature_returns_unauthorized() {
        let secret = "secret";
        let body = r#"{"action":"assigned","issue":{"number":1,"title":"t","assignees":[]},"sender":{"login":"a"},"repository":{"full_name":"owner/repo"}}"#;
        let wrong_sig = "sha256=0000000000000000000000000000000000000000000000000000000000000000";

        let result = handle_github_webhook(wrong_sig, GITHUB_ISSUES, body.as_bytes(), secret);
        assert!(matches!(result, Err(WebhookError::Unauthorized(_))));
    }

    #[test]
    fn test_handle_github_webhook_unknown_event_returns_no_matching_trigger() {
        let secret = "secret";
        let body = b"{\"action\":\"assigned\"}";
        let signature = make_signature(body, secret);

        // "push" is unknown — parse_github_event returns UnknownEventType,
        // which maps to NoMatchingTrigger
        let result = handle_github_webhook(&signature, GITHUB_PUSH, body, secret);
        assert!(matches!(
            result,
            Err(WebhookError::NoMatchingTrigger { .. })
        ));
    }

    #[test]
    fn test_handle_github_webhook_no_matching_action_returns_no_matching_trigger() {
        let secret = "secret";
        // issues + action "opened" should not map to any trigger
        let body = r#"{"action":"opened","issue":{"number":1,"title":"t","assignees":[]},"sender":{"login":"a"},"repository":{"full_name":"owner/repo"}}"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_ISSUES, payload, secret);
        assert!(matches!(
            result,
            Err(WebhookError::NoMatchingTrigger { .. })
        ));
    }

    #[test]
    fn test_handle_github_webhook_malformed_json_returns_bad_request() {
        let secret = "secret";
        let body = b"not json";
        let signature = make_signature(body, secret);

        let result = handle_github_webhook(&signature, "issues", body, secret);
        assert!(matches!(result, Err(WebhookError::BadRequest(_))));
    }

    #[test]
    fn test_handle_github_webhook_invalid_signature_format_returns_unauthorized() {
        let secret = "secret";
        let body = b"{}";
        // Missing "sha256=" prefix
        let bad_sig = "abcdef0123456789";

        let result = handle_github_webhook(bad_sig, GITHUB_ISSUES, body, secret);
        assert!(matches!(result, Err(WebhookError::Unauthorized(_))));
    }

    // --- PR comment via issue_comment event tests ---

    #[test]
    fn test_pr_review_comment_should_not_trigger_issue_mention() {
        // An issue_comment event on a PR should map to
        // GithubPullRequestCommentMention, not GithubIssueCommentMention.
        let body = r#"{
            "action": "created",
            "comment": {
                "id": 12345,
                "body": "This is a PR comment"
            },
            "issue": {
                "number": 42,
                "title": "Some PR",
                "assignees": []
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).expect("Should map to a trigger");

        assert!(
            matches!(trigger, TriggerType::GithubPullRequestCommentMention { .. }),
            "PR review comments via issue_comment should trigger GithubPullRequestCommentMention, got {:?}",
            trigger
        );
    }

    #[test]
    fn test_issue_comment_without_pr_should_trigger_issue_mention() {
        // An issue_comment event without a pull_request field should still
        // map to GithubIssueCommentMention (genuine issue comment).
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
            "sender": {"login": "charlie"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        let trigger = map_to_trigger_event(&event).expect("Should map to a trigger");

        assert!(
            matches!(trigger, TriggerType::GithubIssueCommentMention { .. }),
            "Issue comments without pull_request should trigger GithubIssueCommentMention, got {:?}",
            trigger
        );
    }

    #[test]
    fn test_parse_issue_comment_with_pull_request() {
        // Verify that payload parsing correctly captures the pull_request field.
        let body = r#"{
            "action": "created",
            "comment": {
                "id": 12345,
                "body": "PR comment"
            },
            "issue": {
                "number": 42,
                "title": "Some PR",
                "assignees": []
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;

        let event = parse_github_event("issue_comment", body.as_bytes()).unwrap();
        assert_eq!(event.event_type, "issue_comment");
        assert_eq!(event.action, "created");

        if let GitHubPayload::IssueComment(payload) = &event.payload {
            assert!(payload.pull_request.is_some());
            assert_eq!(payload.pull_request.as_ref().unwrap().number, 7);
        } else {
            panic!("Expected IssueCommentPayload");
        }
    }

    #[test]
    fn test_handle_github_webhook_pr_review_actor_is_reviewer() {
        let secret = "test-secret";
        let body = r#"{
            "action": "submitted",
            "review": {
                "id": 999,
                "body": "LGTM",
                "user": {"login": "actual-reviewer"}
            },
            "pull_request": {"number": 7},
            "sender": {"login": "event-sender"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_PULL_REQUEST_REVIEW, payload, secret);
        assert!(result.is_ok());
        let event = result.unwrap();

        // The actor should be the review author, not the event sender
        assert_eq!(event.actor, "actual-reviewer");
    }

    #[test]
    fn test_handle_github_webhook_pr_comment_via_issue_comment() {
        // Full pipeline test: issue_comment on a PR should produce
        // PR-style event_id and variables.
        let secret = "test-secret";
        let body = r#"{
            "action": "created",
            "comment": {
                "id": 99999,
                "body": "PR comment via issue_comment"
            },
            "issue": {
                "number": 42,
                "title": "Some PR",
                "assignees": []
            },
            "pull_request": {
                "number": 7
            },
            "sender": {"login": "reviewer"},
            "repository": {"full_name": "owner/repo"}
        }"#;
        let payload = body.as_bytes();
        let signature = make_signature(payload, secret);

        let result = handle_github_webhook(&signature, GITHUB_ISSUE_COMMENT, payload, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GithubPullRequestCommentMention { .. }
        ));
        // Should use PR-style event_id format, not issue-style
        assert_eq!(event.event_id, "pr-7-comment-99999");
        assert_eq!(event.variables.get("pr_number").unwrap(), "7");
        assert_eq!(event.variables.get("comment_id").unwrap(), "99999");
        // Should NOT have issue_number
        assert!(!event.variables.contains_key("issue_number"));
    }
}
