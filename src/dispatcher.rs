//! Dispatcher deduplication data structures and in-memory logic.
//!
//! This module provides the core deduplication mechanism to prevent concurrent
//! or repeated execution of the same webhook event. Three `HashSet`s track
//! event states: `in_flight` (currently processing), `completed` (successfully
//! finished), and `permanently_failed` (terminally failed).
//!
//! The dedup key format is `{owner}/{repo}/{workspace_id}`, where
//! `workspace_id` is extracted from the `TriggerEvent` based on the event type:
//! - Issue events: the issue number
//! - PR review events: `{pr_number}_review-{review_id}`
//! - Issue comment events: the issue number
//! - PR review comment events: `{pr_number}_comment-{comment_id}`
//!
//! Thread-safe access is provided via `SharedDedupSets` (`Arc<RwLock<DedupSets>>`).

use std::collections::HashSet;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::webhook::TriggerEvent;
use crate::workflow::TriggerType;

/// Three-set deduplication tracker for webhook events.
///
/// Events transition through the following states:
/// 1. `in_flight` — event is currently being processed
/// 2. `completed` — event finished successfully (terminal state)
/// 3. `permanently_failed` — event failed and will not be retried (terminal state)
///
/// An event is considered a duplicate if its key exists in any of the three sets.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
pub struct DedupSets {
    pub in_flight: HashSet<String>,
    pub completed: HashSet<String>,
    pub permanently_failed: HashSet<String>,
}

#[allow(dead_code)]
impl DedupSets {
    /// Create a new empty `DedupSets`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a key exists in any of the three dedup sets.
    ///
    /// Returns `true` if the event is currently being processed, has already
    /// completed, or has permanently failed — i.e. it should not be processed again.
    pub fn is_duplicate(&self, key: &str) -> bool {
        self.in_flight.contains(key)
            || self.completed.contains(key)
            || self.permanently_failed.contains(key)
    }

    /// Mark an event as in-flight (currently being processed).
    ///
    /// Call this before starting event processing to prevent concurrent execution.
    pub fn mark_in_flight(&mut self, key: &str) {
        self.in_flight.insert(key.to_string());
    }

    /// Move an event from `in_flight` to `completed`.
    ///
    /// Call this when event processing finishes successfully.
    pub fn mark_completed(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.completed.insert(key.to_string());
    }

    /// Move an event from `in_flight` to `permanently_failed`.
    ///
    /// Call this when event processing fails and should not be retried.
    pub fn mark_failed(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.permanently_failed.insert(key.to_string());
    }

    /// Remove an event from `in_flight` without moving it to a terminal state.
    ///
    /// Use this to clean up in-flight tracking when the event should be
    /// re-processed (e.g. a transient failure that allows retries).
    pub fn remove_in_flight(&mut self, key: &str) {
        self.in_flight.remove(key);
    }
}

/// Thread-safe wrapper around `DedupSets` using `Arc<RwLock<...>>`.
///
/// This type is `Clone` (cheaply, via `Arc` clone) and can be shared across
/// async tasks. Use `read()` for `is_duplicate` checks and `write()` for
/// state transitions.
#[allow(dead_code)]
pub type SharedDedupSets = Arc<RwLock<DedupSets>>;

/// Build a dedup key from owner, repo, and workspace ID.
///
/// The format is `{owner}/{repo}/{workspace_id}`, e.g.
/// `mintybasil/yoke/42` or `internal-team/backend-service/7_review-999`.
#[allow(dead_code)]
pub fn build_dedup_key(owner: &str, repo: &str, workspace_id: &str) -> String {
    format!("{}/{}/{}", owner, repo, workspace_id)
}

/// Extract a workspace ID from a `TriggerEvent` for deduplication.
///
/// The workspace ID identifies the specific work context for an event:
/// - GitHub issue assigned: the issue number (e.g. `42`)
/// - GitHub issue comment: the issue number (e.g. `42`)
/// - GitHub PR review: `{pr_number}_review-{review_id}` (e.g. `7_review-999`)
/// - GitHub PR review comment: `{pr_number}_comment-{comment_id}` (e.g. `7_comment-555`)
/// - GitLab issue assigned: the issue IID (e.g. `7`)
/// - GitLab issue mention: the issue IID (e.g. `7`)
/// - GitLab MR review: `{mr_iid}_review-{note_id}` (e.g. `12_review-150`)
/// - GitLab MR comment: `{mr_iid}_comment-{note_id}` (e.g. `12_comment-250`)
#[allow(dead_code)]
pub fn extract_workspace_id(event: &TriggerEvent) -> String {
    match &event.trigger_type {
        // GitHub: event_id format is "issue-{number}" or "issue-{number}-comment-{id}"
        // workspace ID is just the issue number
        TriggerType::GithubIssueAssigned { .. } | TriggerType::GithubIssueCommentMention { .. } => {
            extract_github_issue_workspace_id(&event.event_id)
        }
        // GitHub: event_id format is "pr-{pr_number}-review-{review_id}"
        // or "pr-{pr_number}-comment-{comment_id}"
        // workspace ID is "{pr_number}_review-{review_id}" or "{pr_number}_comment-{comment_id}"
        TriggerType::GithubPullRequestReview { .. }
        | TriggerType::GithubPullRequestCommentMention { .. } => {
            extract_github_pr_workspace_id(&event.event_id)
        }
        // GitLab: event_id format is "issue-{iid}" or "issue-{iid}-note-{note_id}"
        // workspace ID is just the issue IID
        TriggerType::GitlabIssueAssigned { .. } | TriggerType::GitlabIssueMention { .. } => {
            extract_gitlab_issue_workspace_id(&event.event_id)
        }
        // GitLab: event_id format is "mr-{iid}-review-{note_id}" or "mr-{iid}-comment-{note_id}"
        // workspace ID is "{iid}_review-{note_id}" or "{iid}_comment-{note_id}"
        TriggerType::GitlabMergeRequestReview { .. }
        | TriggerType::GitlabMergeRequestCommentMention { .. } => {
            extract_gitlab_mr_workspace_id(&event.event_id)
        }
    }
}

/// For GitHub issue events, extract the issue number as workspace ID.
/// Input: "issue-42" or "issue-42-comment-12345" -> "42"
#[allow(dead_code)]
fn extract_github_issue_workspace_id(event_id: &str) -> String {
    event_id
        .strip_prefix("issue-")
        .map(|s| {
            s.split_once('-')
                .map_or(s.to_string(), |(num, _)| num.to_string())
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// For GitHub PR events, convert event_id to workspace ID format.
/// Input: "pr-7-review-999" -> "7_review-999"
/// Input: "pr-7-comment-555" -> "7_comment-555"
#[allow(dead_code)]
fn extract_github_pr_workspace_id(event_id: &str) -> String {
    event_id
        .strip_prefix("pr-")
        .map(|rest| {
            if let Some((pr_num, after)) = rest.split_once('-') {
                format!("{}_{}", pr_num, after)
            } else {
                rest.to_string()
            }
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// For GitLab issue events, extract the issue number as workspace ID.
/// Input: "issue-7" or "issue-7-note-99" -> "7"
#[allow(dead_code)]
fn extract_gitlab_issue_workspace_id(event_id: &str) -> String {
    event_id
        .strip_prefix("issue-")
        .map(|s| {
            s.split_once('-')
                .map_or(s.to_string(), |(num, _)| num.to_string())
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// For GitLab MR events, convert event_id to workspace ID format.
/// Input: "mr-12-review-150" -> "12_review-150"
/// Input: "mr-12-comment-250" -> "12_comment-250"
#[allow(dead_code)]
fn extract_gitlab_mr_workspace_id(event_id: &str) -> String {
    event_id
        .strip_prefix("mr-")
        .map(|rest| {
            if let Some((mr_iid, after)) = rest.split_once('-') {
                format!("{}_{}", mr_iid, after)
            } else {
                rest.to_string()
            }
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// Create a new `SharedDedupSets` (wrapped in `Arc<RwLock<...>>`).
#[allow(dead_code)]
pub fn new_dedup_sets() -> SharedDedupSets {
    Arc::new(RwLock::new(DedupSets::new()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::TriggerType;

    // --- DedupSets struct tests ---

    #[test]
    fn test_dedup_sets_new_is_empty() {
        let sets = DedupSets::new();
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    #[test]
    fn test_dedup_sets_default_is_empty() {
        let sets = DedupSets::default();
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    // --- is_duplicate tests ---

    #[test]
    fn test_is_duplicate_empty_sets() {
        let sets = DedupSets::new();
        assert!(!sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_in_flight() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_completed() {
        let mut sets = DedupSets::new();
        sets.mark_completed("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_permanently_failed() {
        let mut sets = DedupSets::new();
        sets.mark_failed("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_different_keys() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        assert!(!sets.is_duplicate("owner/repo/43"));
    }

    // --- mark_in_flight tests ---

    #[test]
    fn test_mark_in_flight_adds_to_set() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        assert!(!sets.in_flight.contains(key));
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));
    }

    #[test]
    fn test_mark_in_flight_prevents_concurrent() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        // Second call is idempotent
        sets.mark_in_flight("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    // --- mark_completed tests ---

    #[test]
    fn test_mark_completed_moves_from_in_flight() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));
        assert!(!sets.completed.contains(key));

        sets.mark_completed(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(sets.completed.contains(key), "key should be in completed");
        assert!(
            sets.is_duplicate(key),
            "completed key should still be a duplicate"
        );
    }

    #[test]
    fn test_mark_completed_idempotent() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        sets.mark_completed(key);
        sets.mark_completed(key); // second call is a no-op
        assert!(sets.completed.contains(key));
        assert!(!sets.in_flight.contains(key));
    }

    // --- mark_failed tests ---

    #[test]
    fn test_mark_failed_moves_from_in_flight() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);

        sets.mark_failed(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(
            sets.permanently_failed.contains(key),
            "key should be in permanently_failed"
        );
        assert!(
            sets.is_duplicate(key),
            "failed key should still be a duplicate"
        );
    }

    #[test]
    fn test_mark_failed_idempotent() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        sets.mark_failed(key);
        sets.mark_failed(key); // second call is a no-op
        assert!(sets.permanently_failed.contains(key));
    }

    // --- remove_in_flight tests ---

    #[test]
    fn test_remove_in_flight_removes_key() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));

        sets.remove_in_flight(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(
            !sets.is_duplicate(key),
            "key should no longer be a duplicate"
        );
    }

    #[test]
    fn test_remove_in_flight_nonexistent_is_noop() {
        let mut sets = DedupSets::new();
        sets.remove_in_flight("owner/repo/42");
        // Should not panic
        assert!(sets.in_flight.is_empty());
    }

    // --- Full state transition tests ---

    #[test]
    fn test_full_lifecycle_in_flight_to_completed() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Start: not a duplicate
        assert!(!sets.is_duplicate(key));

        // Mark in-flight
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));
        assert!(sets.in_flight.contains(key));

        // Mark completed
        sets.mark_completed(key);
        assert!(sets.is_duplicate(key));
        assert!(!sets.in_flight.contains(key));
        assert!(sets.completed.contains(key));
    }

    #[test]
    fn test_full_lifecycle_in_flight_to_failed() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Mark in-flight
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));

        // Mark failed
        sets.mark_failed(key);
        assert!(sets.is_duplicate(key));
        assert!(!sets.in_flight.contains(key));
        assert!(sets.permanently_failed.contains(key));
    }

    #[test]
    fn test_in_flight_then_rollback() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Mark in-flight, then roll back (transient failure, allow retry)
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));

        sets.remove_in_flight(key);
        assert!(!sets.is_duplicate(key));
        assert!(sets.in_flight.is_empty());
    }

    // --- build_dedup_key tests ---

    #[test]
    fn test_build_dedup_key_basic() {
        let key = build_dedup_key("owner", "repo", "42");
        assert_eq!(key, "owner/repo/42");
    }

    #[test]
    fn test_build_dedup_key_with_workspace_id() {
        let key = build_dedup_key("mintybasil", "yoke", "7_review-999");
        assert_eq!(key, "mintybasil/yoke/7_review-999");
    }

    #[test]
    fn test_build_dedup_key_with_namespace() {
        let key = build_dedup_key("internal-team", "backend-service", "42");
        assert_eq!(key, "internal-team/backend-service/42");
    }

    // --- extract_workspace_id tests ---

    fn make_trigger_event(trigger_type: TriggerType, event_id: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_type,
            repo_path: "owner/repo".to_string(),
            event_id: event_id.to_string(),
        }
    }

    #[test]
    fn test_extract_workspace_id_github_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned {
                assigned_to: None,
                allowed_users: None,
            },
            "issue-42",
        );
        assert_eq!(extract_workspace_id(&event), "42");
    }

    #[test]
    fn test_extract_workspace_id_github_issue_comment() {
        let event = make_trigger_event(
            TriggerType::GithubIssueCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "issue-42-comment-12345",
        );
        assert_eq!(extract_workspace_id(&event), "42");
    }

    #[test]
    fn test_extract_workspace_id_github_pr_review() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestReview {
                allowed_users: None,
            },
            "pr-7-review-999",
        );
        assert_eq!(extract_workspace_id(&event), "7_review-999");
    }

    #[test]
    fn test_extract_workspace_id_github_pr_review_comment() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "pr-7-comment-555",
        );
        assert_eq!(extract_workspace_id(&event), "7_comment-555");
    }

    #[test]
    fn test_extract_workspace_id_gitlab_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueAssigned { assigned_to: None },
            "issue-7",
        );
        assert_eq!(extract_workspace_id(&event), "7");
    }

    #[test]
    fn test_extract_workspace_id_gitlab_issue_mention() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "issue-7-note-99",
        );
        assert_eq!(extract_workspace_id(&event), "7");
    }

    #[test]
    fn test_extract_workspace_id_gitlab_mr_review() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestReview {
                allowed_users: None,
            },
            "mr-12-review-150",
        );
        assert_eq!(extract_workspace_id(&event), "12_review-150");
    }

    #[test]
    fn test_extract_workspace_id_gitlab_mr_comment() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "mr-12-comment-250",
        );
        assert_eq!(extract_workspace_id(&event), "12_comment-250");
    }

    // --- New dedup sets helper ---

    #[test]
    fn test_new_dedup_sets() {
        let sets = new_dedup_sets();
        let read_guard = sets.try_read().unwrap();
        assert!(read_guard.in_flight.is_empty());
        assert!(read_guard.completed.is_empty());
        assert!(read_guard.permanently_failed.is_empty());
    }

    // --- Async RwLock integration tests ---

    #[tokio::test]
    async fn test_shared_dedup_sets_async_read_write() {
        let sets = new_dedup_sets();

        // Write
        {
            let mut guard = sets.write().await;
            guard.mark_in_flight("owner/repo/42");
        }

        // Read
        {
            let guard = sets.read().await;
            assert!(guard.is_duplicate("owner/repo/42"));
            assert!(!guard.is_duplicate("owner/repo/43"));
        }

        // Transition
        {
            let mut guard = sets.write().await;
            guard.mark_completed("owner/repo/42");
        }

        // Verify
        {
            let guard = sets.read().await;
            assert!(guard.is_duplicate("owner/repo/42"));
            assert!(guard.completed.contains("owner/repo/42"));
            assert!(!guard.in_flight.contains("owner/repo/42"));
        }
    }

    // --- build_dedup_key + extract_workspace_id integration ---

    #[test]
    fn test_integration_build_key_with_workspace_id() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned {
                assigned_to: None,
                allowed_users: None,
            },
            "issue-42",
        );
        let workspace_id = extract_workspace_id(&event);
        let key = build_dedup_key("mintybasil", "yoke", &workspace_id);
        assert_eq!(key, "mintybasil/yoke/42");
    }

    #[test]
    fn test_integration_pr_review_dedup_key() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestReview {
                allowed_users: None,
            },
            "pr-7-review-999",
        );
        let workspace_id = extract_workspace_id(&event);
        let key = build_dedup_key("mintybasil", "yoke", &workspace_id);
        assert_eq!(key, "mintybasil/yoke/7_review-999");
    }

    #[test]
    fn test_integration_gitlab_mr_dedup_key() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestReview {
                allowed_users: None,
            },
            "mr-12-review-150",
        );
        let workspace_id = extract_workspace_id(&event);
        let key = build_dedup_key("internal-team", "backend-service", &workspace_id);
        assert_eq!(key, "internal-team/backend-service/12_review-150");
    }
}
