//! Dispatcher deduplication data structures, in-memory logic, and persistence.
//!
//! This module provides the core deduplication mechanism to prevent concurrent
//! or repeated execution of the same webhook event. Three `HashSet`s track
//! event states: `in_flight` (currently processing), `completed` (successfully
//! finished), and `permanently_failed` (terminally failed).
//!
//! The dedup key format is `{owner}/{repo}/{event_id}`, where
//! `event_id` is extracted from the `TriggerEvent` based on the event type:
//! - Issue events: the issue number
//! - PR review events: `{pr_number}_review-{review_id}`
//! - Issue comment events: the issue number
//! - PR review comment events: `{pr_number}_comment-{comment_id}`
//!
//! Thread-safe access is provided via `SharedDedupSets` (`Arc<RwLock<DedupSets>>`).
//!
//! Persistence uses atomic file writes (write to `.tmp`, then `rename`) to
//! prevent data corruption on crash. On startup, `load_persistence` reads
//! `completed.json` and `failed.json` from the work directory, gracefully
//! handling missing or corrupted files.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::RwLock;

use crate::webhook::TriggerEvent;
use crate::workflow::TriggerType;

/// A record of a permanently failed event, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct FailedEntry {
    /// The dedup key of the failed event (e.g. `owner/repo/42`).
    pub key: String,
    /// When the failure occurred.
    pub timestamp: SystemTime,
    /// Description of the error that caused the failure.
    pub error: String,
}

/// Errors that can occur during persistence operations.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum PersistenceError {
    /// An I/O error occurred reading or writing a file.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

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

/// Build a dedup key from owner, repo, and event ID.
///
/// The format is `{owner}/{repo}/{event_id}`, e.g.
/// `mintybasil/yoke/42` or `internal-team/backend-service/7_review-999`.
#[allow(dead_code)]
pub fn build_dedup_key(owner: &str, repo: &str, event_id: &str) -> String {
    format!("{}/{}/{}", owner, repo, event_id)
}

/// Extract an event ID from a `TriggerEvent` for deduplication.
///
/// The event ID identifies the specific work context for an event:
/// - GitHub issue assigned: the issue number (e.g. `42`)
/// - GitHub issue comment: the issue number (e.g. `42`)
/// - GitHub PR review: `{pr_number}_review-{review_id}` (e.g. `7_review-999`)
/// - GitHub PR review comment: `{pr_number}_comment-{comment_id}` (e.g. `7_comment-555`)
/// - GitLab issue assigned: the issue IID (e.g. `7`)
/// - GitLab issue mention: the issue IID (e.g. `7`)
/// - GitLab MR review: `{mr_iid}_review-{note_id}` (e.g. `12_review-150`)
/// - GitLab MR comment: `{mr_iid}_comment-{note_id}` (e.g. `12_comment-250`)
#[allow(dead_code)]
pub fn extract_event_id(event: &TriggerEvent) -> String {
    match &event.trigger_type {
        // GitHub: event_id format is "issue-{number}" or "issue-{number}-comment-{id}"
        // event ID is just the issue number
        TriggerType::GithubIssueAssigned { .. } | TriggerType::GithubIssueCommentMention { .. } => {
            extract_github_issue_event_id(&event.event_id)
        }
        // GitHub: event_id format is "pr-{pr_number}-review-{review_id}"
        // or "pr-{pr_number}-comment-{comment_id}"
        // event ID is "{pr_number}_review-{review_id}" or "{pr_number}_comment-{comment_id}"
        TriggerType::GithubPullRequestReview { .. }
        | TriggerType::GithubPullRequestCommentMention { .. } => {
            extract_github_pr_event_id(&event.event_id)
        }
        // GitLab: event_id format is "issue-{iid}" or "issue-{iid}-note-{note_id}"
        // event ID is just the issue IID
        TriggerType::GitlabIssueAssigned { .. } | TriggerType::GitlabIssueMention { .. } => {
            extract_gitlab_issue_event_id(&event.event_id)
        }
        // GitLab: event_id format is "mr-{iid}-review-{note_id}" or "mr-{iid}-comment-{note_id}"
        // event ID is "{iid}_review-{note_id}" or "{iid}_comment-{note_id}"
        TriggerType::GitlabMergeRequestReview { .. }
        | TriggerType::GitlabMergeRequestCommentMention { .. } => {
            extract_gitlab_mr_event_id(&event.event_id)
        }
    }
}

/// For GitHub issue events, extract the issue number as event ID.
/// Input: "issue-42" or "issue-42-comment-12345" -> "42"
#[allow(dead_code)]
fn extract_github_issue_event_id(event_id: &str) -> String {
    event_id
        .strip_prefix("issue-")
        .map(|s| {
            s.split_once('-')
                .map_or(s.to_string(), |(num, _)| num.to_string())
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// For GitHub PR events, extract event ID from the TriggerEvent's event_id.
/// Input: "pr-7-review-999" -> "7_review-999"
/// Input: "pr-7-comment-555" -> "7_comment-555"
#[allow(dead_code)]
fn extract_github_pr_event_id(event_id: &str) -> String {
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

/// For GitLab issue events, extract the issue number as event ID.
/// Input: "issue-7" or "issue-7-note-99" -> "7"
#[allow(dead_code)]
fn extract_gitlab_issue_event_id(event_id: &str) -> String {
    event_id
        .strip_prefix("issue-")
        .map(|s| {
            s.split_once('-')
                .map_or(s.to_string(), |(num, _)| num.to_string())
        })
        .unwrap_or_else(|| event_id.to_string())
}

/// For GitLab MR events, extract event ID from the TriggerEvent's event_id.
/// Input: "mr-12-review-150" -> "12_review-150"
/// Input: "mr-12-comment-250" -> "12_comment-250"
#[allow(dead_code)]
fn extract_gitlab_mr_event_id(event_id: &str) -> String {
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

// ---------------------------------------------------------------------------
// Persistence: loading and saving dedup sets to JSON files
// ---------------------------------------------------------------------------

/// Load and deserialize a JSON dedup file.
///
/// Returns `Err(PersistenceError::Io(NotFound))` if the file does not exist.
/// Returns `Err(PersistenceError::Json(_))` if the file contains invalid JSON.
fn load_dedup_file<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, PersistenceError> {
    if !path.exists() {
        return Err(PersistenceError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("File not found: {}", path.display()),
        )));
    }
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(PersistenceError::Json)
}

/// Save data to a JSON file using atomic writes.
///
/// Writes the data to a `.tmp` file first, then renames it to the target path.
/// The rename operation is atomic on most filesystems, preventing partial writes
/// on crash.
fn save_dedup_file<T: Serialize>(path: &Path, entries: &T) -> Result<(), PersistenceError> {
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(entries)?;
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

#[allow(dead_code)]
impl DedupSets {
    /// Persist the `completed` set to `completed.json` in the given directory.
    ///
    /// Uses atomic file writes (`.tmp` + `rename`) to prevent data corruption.
    pub fn persist_completed(&self, workdir: &Path) -> Result<(), PersistenceError> {
        let path = workdir.join("completed.json");
        save_dedup_file(&path, &self.completed)
    }

    /// Append a failed entry to `failed.json` in the given directory.
    ///
    /// Loads existing failed entries, appends the new one, and atomically
    /// rewrites the file. On missing or corrupted `failed.json`, starts
    /// with an empty list.
    pub fn persist_failed(
        &self,
        workdir: &Path,
        entry: &FailedEntry,
    ) -> Result<(), PersistenceError> {
        let path = workdir.join("failed.json");
        let mut failed: Vec<FailedEntry> = load_dedup_file(&path).unwrap_or_default();
        failed.push(entry.clone());
        save_dedup_file(&path, &failed)
    }
}

/// Load dedup persistence state from the work directory.
///
/// Reads `completed.json` and `failed.json` from `workdir`. Missing files are
/// treated as empty sets (no error). Corrupted files produce a warning on
/// stderr and are treated as empty sets. The `in_flight` set is always empty
/// on startup (in-flight state is transient).
#[allow(dead_code)]
pub fn load_persistence(workdir: &Path) -> DedupSets {
    let completed_path = workdir.join("completed.json");
    let failed_path = workdir.join("failed.json");

    let completed = load_dedup_file::<HashSet<String>>(&completed_path).unwrap_or_else(|e| {
        if !matches!(
            &e,
            PersistenceError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
        ) {
            eprintln!("Warning: Corrupted completed.json: {e}");
        }
        HashSet::new()
    });

    let failed_entries = load_dedup_file::<Vec<FailedEntry>>(&failed_path).unwrap_or_else(|e| {
        if !matches!(
            &e,
            PersistenceError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
        ) {
            eprintln!("Warning: Corrupted failed.json: {e}");
        }
        Vec::new()
    });

    let permanently_failed = failed_entries.into_iter().map(|e| e.key).collect();

    DedupSets {
        in_flight: HashSet::new(),
        completed,
        permanently_failed,
    }
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
    fn test_build_dedup_key_with_event_id() {
        let key = build_dedup_key("mintybasil", "yoke", "7_review-999");
        assert_eq!(key, "mintybasil/yoke/7_review-999");
    }

    #[test]
    fn test_build_dedup_key_with_namespace() {
        let key = build_dedup_key("internal-team", "backend-service", "42");
        assert_eq!(key, "internal-team/backend-service/42");
    }

    // --- extract_event_id tests ---

    fn make_trigger_event(trigger_type: TriggerType, event_id: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_type,
            repo_path: "owner/repo".to_string(),
            event_id: event_id.to_string(),
        }
    }

    #[test]
    fn test_extract_event_id_github_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned {
                assigned_to: None,
                allowed_users: None,
            },
            "issue-42",
        );
        assert_eq!(extract_event_id(&event), "42");
    }

    #[test]
    fn test_extract_event_id_github_issue_comment() {
        let event = make_trigger_event(
            TriggerType::GithubIssueCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "issue-42-comment-12345",
        );
        assert_eq!(extract_event_id(&event), "42");
    }

    #[test]
    fn test_extract_event_id_github_pr_review() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestReview {
                allowed_users: None,
            },
            "pr-7-review-999",
        );
        assert_eq!(extract_event_id(&event), "7_review-999");
    }

    #[test]
    fn test_extract_event_id_github_pr_review_comment() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "pr-7-comment-555",
        );
        assert_eq!(extract_event_id(&event), "7_comment-555");
    }

    #[test]
    fn test_extract_event_id_gitlab_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueAssigned { assigned_to: None },
            "issue-7",
        );
        assert_eq!(extract_event_id(&event), "7");
    }

    #[test]
    fn test_extract_event_id_gitlab_issue_mention() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "issue-7-note-99",
        );
        assert_eq!(extract_event_id(&event), "7");
    }

    #[test]
    fn test_extract_event_id_gitlab_mr_review() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestReview {
                allowed_users: None,
            },
            "mr-12-review-150",
        );
        assert_eq!(extract_event_id(&event), "12_review-150");
    }

    #[test]
    fn test_extract_event_id_gitlab_mr_comment() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestCommentMention {
                mentioned_user: None,
                allowed_users: None,
            },
            "mr-12-comment-250",
        );
        assert_eq!(extract_event_id(&event), "12_comment-250");
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

    // --- build_dedup_key + extract_event_id integration ---

    #[test]
    fn test_integration_build_key_with_event_id() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned {
                assigned_to: None,
                allowed_users: None,
            },
            "issue-42",
        );
        let event_id = extract_event_id(&event);
        let key = build_dedup_key("mintybasil", "yoke", &event_id);
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
        let event_id = extract_event_id(&event);
        let key = build_dedup_key("mintybasil", "yoke", &event_id);
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
        let event_id = extract_event_id(&event);
        let key = build_dedup_key("internal-team", "backend-service", &event_id);
        assert_eq!(key, "internal-team/backend-service/12_review-150");
    }

    // --- Persistence tests ---

    #[test]
    fn test_load_dedup_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let result: Result<HashSet<String>, _> = load_dedup_file(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PersistenceError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "Expected NotFound error, got: {err:?}"
        );
    }

    #[test]
    fn test_load_dedup_file_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        let data = serde_json::to_string_pretty(&HashSet::from([
            "owner/repo/42".to_string(),
            "owner/repo/43".to_string(),
        ]))
        .unwrap();
        std::fs::write(&path, data).unwrap();

        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains("owner/repo/42"));
        assert!(loaded.contains("owner/repo/43"));
    }

    #[test]
    fn test_load_dedup_file_corrupted_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        std::fs::write(&path, "not valid json{{{").unwrap();

        let result: Result<HashSet<String>, _> = load_dedup_file(&path);
        assert!(result.is_err(), "Expected error for corrupted JSON");
        assert!(
            matches!(result.unwrap_err(), PersistenceError::Json(_)),
            "Expected JSON error"
        );
    }

    #[test]
    fn test_save_dedup_file_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");

        let set: HashSet<String> =
            HashSet::from(["owner/repo/42".to_string(), "owner/repo/99".to_string()]);
        save_dedup_file(&path, &set).unwrap();

        // File should exist
        assert!(path.exists());
        // No .tmp file should remain
        assert!(!path.with_extension("json.tmp").exists());

        // Content should be valid and match
        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded, set);
    }

    #[test]
    fn test_save_dedup_file_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");

        // Save initial set
        let mut set = HashSet::new();
        set.insert("owner/repo/42".to_string());
        save_dedup_file(&path, &set).unwrap();

        // Save updated set
        set.insert("owner/repo/100".to_string());
        save_dedup_file(&path, &set).unwrap();

        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains("owner/repo/42"));
        assert!(loaded.contains("owner/repo/100"));
    }

    #[test]
    fn test_persist_completed() {
        let dir = tempfile::tempdir().unwrap();
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        sets.mark_completed("owner/repo/42");

        sets.persist_completed(dir.path()).unwrap();

        let loaded: HashSet<String> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("completed.json")).unwrap(),
        )
        .unwrap();
        assert!(loaded.contains("owner/repo/42"));
    }

    #[test]
    fn test_persist_failed_appends() {
        let dir = tempfile::tempdir().unwrap();
        let sets = DedupSets::new();

        let entry1 = FailedEntry {
            key: "owner/repo/42".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "timeout".to_string(),
        };
        let entry2 = FailedEntry {
            key: "owner/repo/43".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "connection refused".to_string(),
        };

        sets.persist_failed(dir.path(), &entry1).unwrap();
        sets.persist_failed(dir.path(), &entry2).unwrap();

        let loaded: Vec<FailedEntry> =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("failed.json")).unwrap())
                .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], entry1);
        assert_eq!(loaded[1], entry2);
    }

    #[test]
    fn test_load_persistence_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let sets = load_persistence(dir.path());
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    #[test]
    fn test_load_persistence_valid_files() {
        let dir = tempfile::tempdir().unwrap();

        // Write completed.json
        let completed = HashSet::from(["owner/repo/42".to_string(), "owner/repo/99".to_string()]);
        std::fs::write(
            dir.path().join("completed.json"),
            serde_json::to_string_pretty(&completed).unwrap(),
        )
        .unwrap();

        // Write failed.json
        let failed = vec![FailedEntry {
            key: "owner/repo/7".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "something went wrong".to_string(),
        }];
        std::fs::write(
            dir.path().join("failed.json"),
            serde_json::to_string_pretty(&failed).unwrap(),
        )
        .unwrap();

        let sets = load_persistence(dir.path());
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.contains("owner/repo/42"));
        assert!(sets.completed.contains("owner/repo/99"));
        assert!(sets.permanently_failed.contains("owner/repo/7"));
        assert_eq!(sets.completed.len(), 2);
        assert_eq!(sets.permanently_failed.len(), 1);
    }

    #[test]
    fn test_load_persistence_corrupted_file_warns() {
        let dir = tempfile::tempdir().unwrap();

        // Write valid completed.json
        let completed: HashSet<String> = HashSet::new();
        std::fs::write(
            dir.path().join("completed.json"),
            serde_json::to_string_pretty(&completed).unwrap(),
        )
        .unwrap();

        // Write corrupted failed.json
        std::fs::write(dir.path().join("failed.json"), "BAD JSON{{").unwrap();

        let sets = load_persistence(dir.path());
        // Corrupted failed.json → empty permanently_failed
        assert!(sets.permanently_failed.is_empty());
        // Valid completed.json → empty but loaded successfully
        assert!(sets.completed.is_empty());
    }

    #[test]
    fn test_roundtrip_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();

        // Build dedup sets and persist
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        sets.mark_completed("owner/repo/42");
        sets.mark_failed("owner/repo/7");

        sets.persist_completed(dir.path()).unwrap();
        let failed_entry = FailedEntry {
            key: "owner/repo/7".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "permanent failure".to_string(),
        };
        sets.persist_failed(dir.path(), &failed_entry).unwrap();

        // Load back
        let loaded = load_persistence(dir.path());
        assert!(loaded.completed.contains("owner/repo/42"));
        assert!(loaded.permanently_failed.contains("owner/repo/7"));
        assert!(loaded.in_flight.is_empty()); // in_flight is always empty on load
    }
}
