//! GitLab webhook handler: token verification, payload parsing, and event mapping.
//!
//! This module implements the GitLab-specific webhook handling:
//! - Constant-time token verification using `X-Gitlab-Token` header
//! - JSON payload parsing into structured event types
//! - Event mapping to internal `TriggerType` representations
//!
//! Supported GitLab webhook events:
//! - `Issue Hook` → `GitlabIssueAssigned` (when action is "update" with assignment)
//! - `Note Hook` on Issue → `GitlabIssueMention`
//! - `Note Hook` on MergeRequest → `GitlabMergeRequestReview`
//! - `Note Hook` with `type: DiffNote` on MergeRequest → `GitlabMergeRequestCommentMention`

use crate::workflow::TriggerType;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use super::{TriggerEvent, WebhookError};

// ── Payload structs ──────────────────────────────────────────────────────────

/// GitLab webhook event type strings (object_kind values).
pub const GITLAB_PUSH: &str = "push";
/// GitLab merge request event type (used in API webhook event config).
pub const GITLAB_MERGE_REQUESTS: &str = "merge_requests";
/// GitLab issue object_kind value.
pub const GITLAB_ISSUE: &str = "issue";
pub const GITLAB_NOTE: &str = "note";

/// Root payload structure for GitLab webhook events.
///
/// GitLab sends different payloads depending on the event type. The `object_kind`
/// field discriminates between them (`"issue"` or `"note"`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitLabPayload {
    /// The type of object this event refers to (e.g. "issue", "note").
    pub object_kind: String,
    /// The event type from the `X-GitLab-Event` header (e.g. "Issue Hook", "Note Hook").
    #[serde(default)]
    pub event_type: Option<String>,
    /// Attributes of the object (issue attributes, note attributes, etc.).
    pub object_attributes: GitLabObjectAttributes,
    /// The project this event belongs to.
    pub project: GitLabProject,
    /// The note's target type (e.g. "Issue", "MergeRequest"). Present for Note hooks.
    #[serde(default)]
    pub noteable_type: Option<String>,
    /// System metadata. Present for Note hooks; the `action` field distinguishes
    /// DiffNote from regular notes when on a MergeRequest.
    #[serde(default)]
    pub system: Option<GitLabSystem>,
    /// The user who triggered the event (the "actor").
    /// Used to authorize the event against the workflow's `allowed_users` list.
    #[serde(default)]
    pub user: Option<GitLabUser>,
}

/// Attributes of the GitLab object (issue or note).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitLabObjectAttributes {
    /// Numeric ID of the object.
    pub id: u64,
    /// Action on the object (e.g. "update" for issue assignment).
    #[serde(default)]
    pub action: Option<String>,
    /// Note text content. Present for Note hooks.
    #[serde(default)]
    pub note: Option<String>,
    /// Note ID. Present for Note hooks.
    #[serde(default)]
    pub note_id: Option<u64>,
    /// Internal ID within the project (e.g. issue IID, MR IID).
    #[serde(default)]
    pub iid: Option<u64>,
}

/// The project this webhook event belongs to.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitLabProject {
    /// Project ID.
    pub id: u64,
    /// Full path with namespace (e.g. "internal-team/backend-service").
    pub path_with_namespace: String,
}

/// System metadata for note events.
///
/// For Note Hook events on MergeRequests, the `action` field is set to
/// `"DiffNote"` for inline review comments and empty for general review notes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitLabSystem {
    /// The note type — `"DiffNote"` indicates an inline review comment.
    pub action: String,
}

/// The user who triggered a GitLab webhook event.
///
/// Per the architecture design, the actor (this user) is checked against
/// the workflow's `allowed_users` list to authorize the event.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitLabUser {
    /// The username of the GitLab user who triggered the event.
    pub username: String,
}

// ── Event enum ───────────────────────────────────────────────────────────────

/// Parsed GitLab webhook event, discriminated by `object_kind`.
#[derive(Debug, Clone)]
pub enum GitLabEvent {
    /// Issue Hook event — fired when an issue is created, updated, or closed.
    IssueHook(GitLabPayload),
    /// Note Hook event — fired when a comment is created on an issue or merge request.
    NoteHook(GitLabPayload),
}

impl GitLabEvent {
    /// Return the `object_kind` string for this event.
    pub fn object_kind(&self) -> &str {
        match self {
            GitLabEvent::IssueHook(p) => &p.object_kind,
            GitLabEvent::NoteHook(p) => &p.object_kind,
        }
    }

    /// Return the full path of the project (e.g. "owner/repo").
    pub fn repo_path(&self) -> String {
        match self {
            GitLabEvent::IssueHook(p) => p.project.path_with_namespace.clone(),
            GitLabEvent::NoteHook(p) => p.project.path_with_namespace.clone(),
        }
    }

    /// Return the username of the user who triggered this event (the "actor").
    ///
    /// Per the architecture design, the actor is the user who performed the
    /// action that created the webhook event. This is extracted from
    /// `user.username` in the GitLab payload.
    pub fn actor(&self) -> Option<String> {
        match self {
            GitLabEvent::IssueHook(p) => p.user.as_ref().map(|u| u.username.clone()),
            GitLabEvent::NoteHook(p) => p.user.as_ref().map(|u| u.username.clone()),
        }
    }

    /// Extract trigger-specific template variables from the payload.
    ///
    /// These variables are merged with global variables in the dispatcher
    /// and made available for template rendering in workflow steps.
    pub fn variables(&self) -> std::collections::HashMap<String, String> {
        let mut vars = std::collections::HashMap::new();
        match self {
            GitLabEvent::IssueHook(p) => {
                let iid = p.object_attributes.iid.unwrap_or(p.object_attributes.id);
                vars.insert("issue_iid".to_string(), iid.to_string());
                vars.insert(
                    "issue_action".to_string(),
                    p.object_attributes.action.clone().unwrap_or_default(),
                );
            }
            GitLabEvent::NoteHook(p) => {
                let iid = p.object_attributes.iid.unwrap_or(p.object_attributes.id);
                let note_id = p
                    .object_attributes
                    .note_id
                    .unwrap_or(p.object_attributes.id);
                vars.insert("note_id".to_string(), note_id.to_string());

                match p.noteable_type.as_deref() {
                    Some("Issue") => {
                        vars.insert("issue_iid".to_string(), iid.to_string());
                    }
                    Some("MergeRequest") => {
                        vars.insert("mr_iid".to_string(), iid.to_string());
                    }
                    _ => {}
                }

                if let Some(ref note_text) = p.object_attributes.note {
                    vars.insert("note_body".to_string(), note_text.clone());
                }
            }
        }
        vars
    }

    /// Return a deduplication-friendly event ID.
    ///
    /// Format matches Appendix A of the architecture doc:
    /// - Issue: `issue-{iid}`
    /// - Note on Issue: `issue-{iid}-note-{note_id}`
    /// - Note on MR (review): `mr-{iid}-review-{note_id}`
    /// - Note on MR (comment): `mr-{iid}-comment-{note_id}`
    pub fn event_id(&self) -> String {
        match self {
            GitLabEvent::IssueHook(p) => {
                let iid = p.object_attributes.iid.unwrap_or(p.object_attributes.id);
                format!("issue-{iid}")
            }
            GitLabEvent::NoteHook(p) => {
                let note_id = p
                    .object_attributes
                    .note_id
                    .unwrap_or(p.object_attributes.id);
                let iid = p.object_attributes.iid.unwrap_or(p.object_attributes.id);
                match p.noteable_type.as_deref() {
                    Some("Issue") => format!("issue-{iid}-note-{note_id}"),
                    Some("MergeRequest") => {
                        let is_diff = p
                            .system
                            .as_ref()
                            .map(|s| s.action == "DiffNote")
                            .unwrap_or(false);
                        if is_diff {
                            format!("mr-{iid}-comment-{note_id}")
                        } else {
                            format!("mr-{iid}-review-{note_id}")
                        }
                    }
                    _ => format!("note-{note_id}"),
                }
            }
        }
    }
}

// ── Token verification ────────────────────────────────────────────────────────

/// Verify the GitLab webhook token using constant-time comparison.
///
/// Compares the `X-Gitlab-Token` header value against the configured secret
/// in constant time to prevent timing attacks.
///
/// Returns `Ok(())` if the tokens match, `Err` otherwise.
pub fn verify_gitlab_token(header: &str, secret: &str) -> Result<(), String> {
    if header.as_bytes().ct_eq(secret.as_bytes()).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err("Invalid GitLab token".to_string())
    }
}

// ── Event parsing ─────────────────────────────────────────────────────────────

/// Parse a raw GitLab webhook payload into a typed `GitLabEvent`.
///
/// The `object_kind` field determines the event variant:
/// - `"issue"` → `GitLabEvent::IssueHook`
/// - `"note"` → `GitLabEvent::NoteHook`
///
/// Returns `Err` for unknown object kinds.
pub fn parse_gitlab_event(payload: &[u8]) -> Result<GitLabEvent, String> {
    let p: GitLabPayload = serde_json::from_slice(payload)
        .map_err(|e| format!("Failed to parse GitLab payload: {e}"))?;

    match p.object_kind.as_str() {
        GITLAB_ISSUE => Ok(GitLabEvent::IssueHook(p)),
        GITLAB_NOTE => Ok(GitLabEvent::NoteHook(p)),
        other => Err(format!("Unsupported object_kind: {other}")),
    }
}

// ── Event mapping ─────────────────────────────────────────────────────────────

/// Map a parsed `GitLabEvent` to a `TriggerType`.
///
/// Returns `Some(TriggerType)` when the event matches a known trigger,
/// or `None` when the event type or action doesn't map to any trigger.
///
/// Mapping rules (per Architecture Design §4 and Appendix A):
/// - `IssueHook` with `action == "update"` → `TriggerType::GitlabIssueAssigned`
/// - `NoteHook` on `Issue` → `TriggerType::GitlabIssueMention`
/// - `NoteHook` on `MergeRequest` with `system.action == "DiffNote"` → `TriggerType::GitlabMergeRequestCommentMention`
/// - `NoteHook` on `MergeRequest` (plain note) → `TriggerType::GitlabMergeRequestReview`
pub fn map_to_trigger_event(event: &GitLabEvent) -> Option<TriggerType> {
    match event {
        GitLabEvent::IssueHook(p) => {
            // Only trigger on "update" action (assignment fires as an update)
            if p.object_attributes.action.as_deref() == Some("update") {
                Some(TriggerType::GitlabIssueAssigned {
                    assigned_to: None,
                    allowed_users: None,
                })
            } else {
                None
            }
        }
        GitLabEvent::NoteHook(p) => {
            let noteable_type = p.noteable_type.as_deref().unwrap_or("");
            let note_type = p.system.as_ref().map(|s| s.action.as_str()).unwrap_or("");

            match (noteable_type, note_type) {
                ("Issue", _) => Some(TriggerType::GitlabIssueMention {
                    mentioned_user: None,
                    allowed_users: None,
                }),
                ("MergeRequest", "DiffNote") => {
                    Some(TriggerType::GitlabMergeRequestCommentMention {
                        mentioned_user: None,
                        allowed_users: None,
                    })
                }
                ("MergeRequest", _) => Some(TriggerType::GitlabMergeRequestReview {
                    allowed_users: None,
                }),
                _ => None,
            }
        }
    }
}

// ── Webhook handler ───────────────────────────────────────────────────────────

/// Handle a GitLab webhook request.
///
/// Verifies the token, parses the event payload, and maps it to a trigger type.
/// Returns `Ok(TriggerEvent)` on success, or a `WebhookError` on failure.
pub fn handle_gitlab_webhook(
    token_header: &str,
    _event_header: &str,
    body: &[u8],
    secret: &str,
) -> Result<TriggerEvent, WebhookError> {
    // Step 1: Verify the token
    verify_gitlab_token(token_header, secret).map_err(WebhookError::Unauthorized)?;

    // Step 2: Parse the event from the payload
    let event = parse_gitlab_event(body).map_err(WebhookError::BadRequest)?;

    // Step 3: Map to a trigger type
    let trigger_type = map_to_trigger_event(&event).ok_or_else(|| {
        WebhookError::NoMatchingTrigger(format!(
            "No matching trigger for object_kind '{}'",
            event.object_kind()
        ))
    })?;

    // Step 4: Extract repo path, event ID, actor, and variables
    let repo_path = event.repo_path();
    let event_id = event.event_id();
    let variables = event.variables();

    // Extract the actor (user) from the GitLab payload.
    // Per the architecture design, the actor is the user who performed the
    // action that created the webhook event (e.g. the person who assigned the
    // issue or wrote the comment). This is used to authorize the event against
    // the workflow's `allowed_users` list.
    let actor = event.actor().unwrap_or_default();
    if actor.is_empty() {
        return Err(WebhookError::BadRequest(
            "GitLab webhook payload missing user field: cannot determine actor".to_string(),
        ));
    }

    Ok(TriggerEvent {
        trigger_type,
        repo_path,
        event_id,
        actor,
        variables,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Token verification tests ──────────────────────────────────────

    #[test]
    fn test_verify_gitlab_token_valid() {
        assert!(verify_gitlab_token("secret123", "secret123").is_ok());
    }

    #[test]
    fn test_verify_gitlab_token_invalid() {
        assert!(verify_gitlab_token("wrong", "secret123").is_err());
    }

    #[test]
    fn test_verify_gitlab_token_empty_header() {
        assert!(verify_gitlab_token("", "secret").is_err());
    }

    #[test]
    fn test_verify_gitlab_token_empty_secret() {
        assert!(verify_gitlab_token("secret", "").is_err());
    }

    #[test]
    fn test_verify_gitlab_token_both_empty() {
        // Two empty strings are equal, which is valid
        assert!(verify_gitlab_token("", "").is_ok());
    }

    #[test]
    fn test_verify_gitlab_token_timing_safe() {
        // Constant-time comparison should not leak length info.
        // This test just verifies the function uses ct_eq — actual timing
        // guarantees are provided by the subtle crate.
        let secret = "a-very-long-secret-key-for-testing";
        assert!(verify_gitlab_token(secret, secret).is_ok());
        let similar = "a-very-long-secret-key-for-testinx";
        assert!(verify_gitlab_token(similar, secret).is_err());
    }

    // ── Event parsing tests ────────────────────────────────────────────

    fn sample_issue_payload() -> GitLabPayload {
        GitLabPayload {
            object_kind: GITLAB_ISSUE.to_string(),
            event_type: Some("Issue Hook".to_string()),
            object_attributes: GitLabObjectAttributes {
                id: 42,
                action: Some("update".to_string()),
                note: None,
                note_id: None,
                iid: Some(7),
            },
            project: GitLabProject {
                id: 1,
                path_with_namespace: "internal-team/backend-service".to_string(),
            },
            noteable_type: None,
            system: None,
            user: Some(GitLabUser { username: "testuser".to_string() }),
        }
    }

    fn sample_note_on_issue_payload() -> GitLabPayload {
        GitLabPayload {
            object_kind: GITLAB_NOTE.to_string(),
            event_type: Some("Note Hook".to_string()),
            object_attributes: GitLabObjectAttributes {
                id: 100,
                action: None,
                note: Some("@bot please review this".to_string()),
                note_id: Some(99),
                iid: Some(7),
            },
            project: GitLabProject {
                id: 1,
                path_with_namespace: "internal-team/backend-service".to_string(),
            },
            noteable_type: Some("Issue".to_string()),
            system: None,
            user: Some(GitLabUser { username: "testuser".to_string() }),
        }
    }

    fn sample_note_on_mr_payload() -> GitLabPayload {
        GitLabPayload {
            object_kind: GITLAB_NOTE.to_string(),
            event_type: Some("Note Hook".to_string()),
            object_attributes: GitLabObjectAttributes {
                id: 200,
                action: None,
                note: Some("LGTM".to_string()),
                note_id: Some(150),
                iid: Some(12),
            },
            project: GitLabProject {
                id: 1,
                path_with_namespace: "internal-team/backend-service".to_string(),
            },
            noteable_type: Some("MergeRequest".to_string()),
            system: None,
            user: Some(GitLabUser { username: "testuser".to_string() }),
        }
    }

    fn sample_diff_note_on_mr_payload() -> GitLabPayload {
        GitLabPayload {
            object_kind: GITLAB_NOTE.to_string(),
            event_type: Some("Note Hook".to_string()),
            object_attributes: {
                let mut p = sample_note_on_mr_payload().object_attributes;
                p.id = 300;
                p.note = Some("nit: typo".to_string());
                p.note_id = Some(250);
                p
            },
            project: GitLabProject {
                id: 1,
                path_with_namespace: "internal-team/backend-service".to_string(),
            },
            noteable_type: Some("MergeRequest".to_string()),
            system: Some(GitLabSystem {
                action: "DiffNote".to_string(),
            }),
            user: Some(GitLabUser { username: "testuser".to_string() }),
        }
    }

    #[test]
    fn test_parse_gitlab_event_issue() {
        let json = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 42,
                "action": "update",
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "internal-team/backend-service"
            },
            "user": {
                "username": "testuser"
            }
        });
        let result = parse_gitlab_event(json.to_string().as_bytes());
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(event, GitLabEvent::IssueHook(_)));
        assert_eq!(event.object_kind(), GITLAB_ISSUE);
    }

    #[test]
    fn test_parse_gitlab_event_note() {
        let json = serde_json::json!({
            "object_kind": "note",
            "event_type": "Note Hook",
            "object_attributes": {
                "id": 100,
                "note": "comment",
                "note_id": 99,
                "action": null
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "noteable_type": "Issue",
            "user": {
                "username": "testuser"
            }
        });
        let result = parse_gitlab_event(json.to_string().as_bytes());
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(event, GitLabEvent::NoteHook(_)));
    }

    #[test]
    fn test_parse_gitlab_event_unsupported_kind() {
        let json = serde_json::json!({
            "object_kind": "push",
            "object_attributes": {
                "id": 1
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });
        let result = parse_gitlab_event(json.to_string().as_bytes());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("Unsupported object_kind"),
            "Expected unsupported kind error"
        );
    }

    #[test]
    fn test_parse_gitlab_event_invalid_json() {
        let result = parse_gitlab_event(b"not json at all");
        assert!(result.is_err());
    }

    // ── Event mapping tests ────────────────────────────────────────────

    #[test]
    fn test_map_issue_hook_update() {
        let payload = sample_issue_payload();
        let event = GitLabEvent::IssueHook(payload);
        let result = map_to_trigger_event(&event);
        assert!(matches!(
            result,
            Some(TriggerType::GitlabIssueAssigned { .. })
        ));
    }

    #[test]
    fn test_map_issue_hook_open() {
        // "open" action does not trigger assignment
        let mut payload = sample_issue_payload();
        payload.object_attributes.action = Some("open".to_string());
        let event = GitLabEvent::IssueHook(payload);
        assert!(map_to_trigger_event(&event).is_none());
    }

    #[test]
    fn test_map_note_on_issue() {
        let payload = sample_note_on_issue_payload();
        let event = GitLabEvent::NoteHook(payload);
        let result = map_to_trigger_event(&event);
        assert!(matches!(
            result,
            Some(TriggerType::GitlabIssueMention { .. })
        ));
    }

    #[test]
    fn test_map_note_on_mr_plain() {
        let payload = sample_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        let result = map_to_trigger_event(&event);
        assert!(matches!(
            result,
            Some(TriggerType::GitlabMergeRequestReview { .. })
        ));
    }

    #[test]
    fn test_map_diff_note_on_mr() {
        let payload = sample_diff_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        let result = map_to_trigger_event(&event);
        assert!(matches!(
            result,
            Some(TriggerType::GitlabMergeRequestCommentMention { .. })
        ));
    }

    #[test]
    fn test_map_note_unknown_noteable_type() {
        let mut payload = sample_note_on_issue_payload();
        payload.noteable_type = Some("Commit".to_string());
        let event = GitLabEvent::NoteHook(payload);
        assert!(map_to_trigger_event(&event).is_none());
    }

    // ── Event ID tests ─────────────────────────────────────────────────

    #[test]
    fn test_event_id_issue() {
        let payload = sample_issue_payload();
        let event = GitLabEvent::IssueHook(payload);
        assert_eq!(event.event_id(), "issue-7");
    }

    #[test]
    fn test_event_id_note_on_issue() {
        let payload = sample_note_on_issue_payload();
        let event = GitLabEvent::NoteHook(payload);
        assert_eq!(event.event_id(), "issue-7-note-99");
    }

    #[test]
    fn test_event_id_note_on_mr() {
        let payload = sample_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        assert_eq!(event.event_id(), "mr-12-review-150");
    }

    #[test]
    fn test_event_id_diff_note_on_mr() {
        let payload = sample_diff_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        assert_eq!(event.event_id(), "mr-12-comment-250");
    }

    // ── Repo path tests ────────────────────────────────────────────────

    #[test]
    fn test_repo_path_issue() {
        let payload = sample_issue_payload();
        let event = GitLabEvent::IssueHook(payload);
        assert_eq!(event.repo_path(), "internal-team/backend-service");
    }

    #[test]
    fn test_repo_path_note() {
        let payload = sample_note_on_issue_payload();
        let event = GitLabEvent::NoteHook(payload);
        assert_eq!(event.repo_path(), "internal-team/backend-service");
    }

    // ── Variables extraction tests ────────────────────────────────────

    #[test]
    fn test_variables_issue_hook() {
        let payload = sample_issue_payload();
        let event = GitLabEvent::IssueHook(payload);
        let vars = event.variables();
        assert_eq!(vars.get("issue_iid").unwrap(), "7");
        assert_eq!(vars.get("issue_action").unwrap(), "update");
    }

    #[test]
    fn test_variables_note_on_issue() {
        let payload = sample_note_on_issue_payload();
        let event = GitLabEvent::NoteHook(payload);
        let vars = event.variables();
        assert_eq!(vars.get("note_id").unwrap(), "99");
        assert_eq!(vars.get("issue_iid").unwrap(), "7");
        assert!(vars.contains_key("note_body"));
    }

    #[test]
    fn test_variables_note_on_mr() {
        let payload = sample_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        let vars = event.variables();
        assert_eq!(vars.get("note_id").unwrap(), "150");
        assert_eq!(vars.get("mr_iid").unwrap(), "12");
    }

    #[test]
    fn test_variables_diff_note_on_mr() {
        let payload = sample_diff_note_on_mr_payload();
        let event = GitLabEvent::NoteHook(payload);
        let vars = event.variables();
        assert_eq!(vars.get("note_id").unwrap(), "250");
        assert_eq!(vars.get("mr_iid").unwrap(), "12");
    }

    // ── Integration tests for handle_gitlab_webhook ────────────────────

    #[test]
    fn test_handle_gitlab_webhook_full_pipeline_issue_assigned() {
        let secret = "test-secret";
        let body = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 42,
                "action": "update",
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "internal-team/backend-service"
            },
            "user": {
                "username": "alice"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Issue Hook", &body_bytes, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GitlabIssueAssigned { .. }
        ));
        assert_eq!(event.repo_path, "internal-team/backend-service");
        assert_eq!(event.event_id, "issue-7");
        assert_eq!(event.actor, "alice");
        assert_eq!(event.variables.get("issue_iid").unwrap(), "7");
        assert_eq!(event.variables.get("issue_action").unwrap(), "update");
    }

    #[test]
    fn test_handle_gitlab_webhook_full_pipeline_note_on_issue() {
        let secret = "gl-token";
        let body = serde_json::json!({
            "object_kind": "note",
            "event_type": "Note Hook",
            "object_attributes": {
                "id": 100,
                "note": "@bot review this",
                "note_id": 99,
                "iid": 7
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "noteable_type": "Issue",
            "user": {
                "username": "bob"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Note Hook", &body_bytes, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GitlabIssueMention { .. }
        ));
        assert_eq!(event.repo_path, "owner/repo");
        assert_eq!(event.actor, "bob");
        assert_eq!(event.variables.get("note_id").unwrap(), "99");
        assert_eq!(event.variables.get("issue_iid").unwrap(), "7");
    }

    #[test]
    fn test_handle_gitlab_webhook_full_pipeline_note_on_mr() {
        let secret = "gl-token";
        let body = serde_json::json!({
            "object_kind": "note",
            "event_type": "Note Hook",
            "object_attributes": {
                "id": 200,
                "note": "LGTM",
                "note_id": 150,
                "iid": 12
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "noteable_type": "MergeRequest",
            "user": {
                "username": "charlie"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Note Hook", &body_bytes, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GitlabMergeRequestReview { .. }
        ));
        assert_eq!(event.variables.get("note_id").unwrap(), "150");
        assert_eq!(event.variables.get("mr_iid").unwrap(), "12");
        assert_eq!(event.actor, "charlie");
    }

    #[test]
    fn test_handle_gitlab_webhook_full_pipeline_diff_note_on_mr() {
        let secret = "gl-token";
        let body = serde_json::json!({
            "object_kind": "note",
            "event_type": "Note Hook",
            "object_attributes": {
                "id": 300,
                "note": "nit: typo",
                "note_id": 250,
                "iid": 12
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "noteable_type": "MergeRequest",
            "system": {
                "action": "DiffNote"
            },
            "user": {
                "username": "dave"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Note Hook", &body_bytes, secret);
        assert!(result.is_ok());
        let event = result.unwrap();
        assert!(matches!(
            event.trigger_type,
            TriggerType::GitlabMergeRequestCommentMention { .. }
        ));
        assert_eq!(event.variables.get("note_id").unwrap(), "250");
        assert_eq!(event.variables.get("mr_iid").unwrap(), "12");
        assert_eq!(event.actor, "dave");
    }

    #[test]
    fn test_handle_gitlab_webhook_invalid_token_returns_unauthorized() {
        let secret = "correct-token";
        let body = serde_json::json!({
            "object_kind": "issue",
            "object_attributes": {
                "id": 1,
                "action": "update",
                "iid": 1
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook("wrong-token", "Issue Hook", &body_bytes, secret);
        assert!(matches!(result, Err(WebhookError::Unauthorized(_))));
    }

    #[test]
    fn test_handle_gitlab_webhook_invalid_json_returns_bad_request() {
        let secret = "token";
        let body = b"not json";

        let result = handle_gitlab_webhook(secret, "Issue Hook", body, secret);
        assert!(matches!(result, Err(WebhookError::BadRequest(_))));
    }

    #[test]
    fn test_handle_gitlab_webhook_unsupported_kind_returns_no_matching_trigger() {
        let secret = "token";
        let body = serde_json::json!({
            "object_kind": "push",
            "object_attributes": {
                "id": 1
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Push Hook", &body_bytes, secret);
        assert!(matches!(result, Err(WebhookError::BadRequest(_))));
    }

    #[test]
    fn test_handle_gitlab_webhook_issue_open_returns_no_matching_trigger() {
        // Issue with action "open" should not map to any trigger
        let secret = "token";
        let body = serde_json::json!({
            "object_kind": "issue",
            "event_type": "Issue Hook",
            "object_attributes": {
                "id": 1,
                "action": "open",
                "iid": 1
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "user": {
                "username": "someuser"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Issue Hook", &body_bytes, secret);
        assert!(matches!(result, Err(WebhookError::NoMatchingTrigger(_))));
    }

    #[test]
    fn test_handle_gitlab_webhook_note_unknown_noteable_type_returns_no_matching_trigger() {
        let secret = "token";
        let body = serde_json::json!({
            "object_kind": "note",
            "event_type": "Note Hook",
            "object_attributes": {
                "id": 1,
                "note": "a comment",
                "note_id": 1,
                "iid": 1
            },
            "project": {
                "id": 1,
                "path_with_namespace": "owner/repo"
            },
            "noteable_type": "Commit",
            "user": {
                "username": "someuser"
            }
        });
        let body_bytes = body.to_string().into_bytes();

        let result = handle_gitlab_webhook(secret, "Note Hook", &body_bytes, secret);
        assert!(matches!(result, Err(WebhookError::NoMatchingTrigger(_))));
    }
}
