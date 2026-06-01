//! Central constants for environment variable names, trigger types,
//! webhook event types, and HTTP header names.
//!
//! Using constants instead of scattered string literals prevents typos,
//! makes refactorings safer, and provides a single place to audit
//! all magic strings in the codebase.

// ---------------------------------------------------------------------------
// Environment variable names
// ---------------------------------------------------------------------------

/// Environment variable names used by Yoke.
pub mod env {
    /// Hermes API key (always required).
    pub const HERMES_API_KEY: &str = "HERMES_API_KEY";
    /// GitHub personal access token (required when platform = "github").
    pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";
    /// GitLab personal access token (required when platform = "gitlab").
    pub const GITLAB_TOKEN: &str = "GITLAB_TOKEN";
    /// Optional webhook secret override (overrides `server.webhook_secret` from config).
    pub const WEBHOOK_SECRET: &str = "WEBHOOK_SECRET";
}

// ---------------------------------------------------------------------------
// Trigger type labels (workflow TOML `trigger.type` values)
// ---------------------------------------------------------------------------

/// Trigger type string labels used in workflow TOML `trigger.type` fields
/// and the [`crate::workflow::TriggerType`] enum.
pub mod triggers {
    // GitHub trigger types
    /// GitHub: issue assigned.
    pub const GITHUB_ISSUE_ASSIGNED: &str = "github_issue_assigned";
    /// GitHub: issue comment mention.
    pub const GITHUB_ISSUE_COMMENT_MENTION: &str = "github_issue_comment_mention";
    /// GitHub: pull request review.
    pub const GITHUB_PULL_REQUEST_REVIEW: &str = "github_pull_request_review";
    /// GitHub: pull request review comment mention.
    pub const GITHUB_PULL_REQUEST_COMMENT_MENTION: &str = "github_pull_request_comment_mention";

    // GitLab trigger types
    /// GitLab: issue assigned.
    pub const GITLAB_ISSUE_ASSIGNED: &str = "gitlab_issue_assigned";
    /// GitLab: issue mention.
    pub const GITLAB_ISSUE_MENTION: &str = "gitlab_issue_mention";
    /// GitLab: merge request review.
    pub const GITLAB_MERGE_REQUEST_REVIEW: &str = "gitlab_merge_request_review";
    /// GitLab: merge request review comment (maps to merge_request_comment_mention).
    pub const GITLAB_MERGE_REQUEST_REVIEW_COMMENT: &str = "gitlab_merge_request_review_comment";
}

// ---------------------------------------------------------------------------
// Webhook event type strings (sent to / received from platform APIs)
// ---------------------------------------------------------------------------

/// GitHub and GitLab webhook event type strings used in API calls
/// and webhook payload parsing.
pub mod webhook_events {
    // GitHub event types (X-GitHub-Event header values & API event names)
    /// GitHub push event.
    pub const GITHUB_PUSH: &str = "push";
    /// GitHub pull request event.
    pub const GITHUB_PULL_REQUEST: &str = "pull_request";
    /// GitHub issues event.
    pub const GITHUB_ISSUES: &str = "issues";
    /// GitHub issue comment event.
    pub const GITHUB_ISSUE_COMMENT: &str = "issue_comment";
    /// GitHub pull request review event.
    pub const GITHUB_PULL_REQUEST_REVIEW: &str = "pull_request_review";
    /// GitHub pull request review comment event.
    pub const GITHUB_PULL_REQUEST_REVIEW_COMMENT: &str = "pull_request_review_comment";

    // GitLab event types (object_kind values)
    /// GitLab push event.
    pub const GITLAB_PUSH: &str = "push";
    /// GitLab merge request event (used in API webhook event config).
    pub const GITLAB_MERGE_REQUESTS: &str = "merge_requests";
    /// GitLab issue object_kind value.
    pub const GITLAB_ISSUE: &str = "issue";
    /// GitLab note object_kind value.
    pub const GITLAB_NOTE: &str = "note";
}

// ---------------------------------------------------------------------------
// HTTP header names (webhook verification and routing)
// ---------------------------------------------------------------------------

/// HTTP header names for webhook authentication and event identification.
pub mod headers {
    /// GitHub HMAC-SHA256 signature header.
    pub const GITHUB_SIGNATURE: &str = "X-Hub-Signature-256";
    /// GitHub event type header.
    pub const GITHUB_EVENT: &str = "X-GitHub-Event";
    /// GitLab webhook token header.
    pub const GITLAB_TOKEN: &str = "X-Gitlab-Token";
    /// GitLab event type header.
    pub const GITLAB_EVENT: &str = "X-Gitlab-Event";
}
