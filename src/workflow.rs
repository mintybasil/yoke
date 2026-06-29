use std::fs;
use std::path::Path;

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

use crate::config::Platform;

/// Trigger type string labels used in workflow TOML `trigger.type` fields
/// and the [`TriggerType`] enum.
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

/// A complete workflow definition loaded from a `.toml` file.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Workflow {
    /// File path this workflow was loaded from (set by `load_workflows`).
    #[serde(default)]
    pub path: String,
    pub trigger: Trigger,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default)]
    pub steps: Vec<Step>,
}

/// Trigger configuration: what events start this workflow.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Trigger {
    pub r#type: String,
    #[serde(default)]
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub mentioned_user: Option<String>,
    #[serde(default)]
    pub allowed_users: Option<Vec<String>>,
}

/// Per-event shallow clone configuration.
///
/// Opt-in: a workflow that needs repository access must explicitly enable
/// `[git] clone = true` in its TOML file. When enabled, the dispatcher
/// performs a per-event shallow clone (`git clone --depth=1 -b <branch>`)
/// into the event's workspace directory.
///
/// The `worktree` field is no longer supported. If a TOML file contains
/// `worktree = true` (or `worktree = false`), loading will fail with a
/// clear error message directing the user to use `clone = true` instead.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(try_from = "GitConfigHelper")]
pub struct GitConfig {
    /// Whether to perform a per-event shallow clone before running the workflow.
    /// When true, the dispatcher clones the repository into the event's
    /// workspace directory using `git clone --depth=1 -b <branch>`.
    /// Each event gets its own isolated clone — no shared `.git` state.
    #[serde(default)]
    pub clone: bool,
    /// The branch to clone when no source branch is available from the webhook
    /// payload (e.g. for issue-assigned triggers). Defaults to `"main"`.
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

/// Helper struct for deserialization that intercepts the removed `worktree` field.
#[derive(Deserialize)]
struct GitConfigHelper {
    #[serde(default)]
    clone: bool,
    #[serde(default = "default_branch")]
    default_branch: String,
    /// Removed field — reject with a clear migration message.
    worktree: Option<IgnoredAny>,
}

impl TryFrom<GitConfigHelper> for GitConfig {
    type Error = String;

    fn try_from(helper: GitConfigHelper) -> Result<Self, Self::Error> {
        if helper.worktree.is_some() {
            return Err("The `worktree` field in [git] is no longer supported. \
                 Use `clone = true` instead for per-event shallow clone behavior."
                .into());
        }
        Ok(GitConfig {
            clone: helper.clone,
            default_branch: helper.default_branch,
        })
    }
}

fn default_branch() -> String {
    "main".to_string()
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            clone: false,
            default_branch: default_branch(),
        }
    }
}

/// A single step in a workflow.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Step {
    pub name: String,
    pub agent: String,
    pub prompt_template: String,
    #[serde(default)]
    pub pre_hooks: Vec<Hook>,
    #[serde(default)]
    pub post_hooks: Vec<Hook>,
}

pub use crate::hooks::Hook;

/// Typed representation of known trigger types, grouped by platform.
///
/// Each variant carries the filter fields required by that trigger type
/// per Appendix A of the architecture design doc.
#[derive(Debug, Clone, PartialEq)]
pub enum TriggerType {
    // GitHub triggers
    GithubIssueAssigned { assigned_to: Option<String> },
    GithubIssueCommentMention { mentioned_user: Option<String> },
    GithubPullRequestReview,
    GithubPullRequestCommentMention { mentioned_user: Option<String> },
    // GitLab triggers
    GitlabIssueAssigned { assigned_to: Option<String> },
    GitlabIssueMention { mentioned_user: Option<String> },
    GitlabMergeRequestReview,
    GitlabMergeRequestCommentMention { mentioned_user: Option<String> },
}

impl std::fmt::Display for TriggerType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TriggerType::GithubIssueAssigned { assigned_to } => {
                write!(
                    f,
                    "GithubIssueAssigned {{ assigned_to: {} }}",
                    assigned_to.as_deref().unwrap_or("None")
                )
            }
            TriggerType::GithubIssueCommentMention { mentioned_user } => {
                write!(
                    f,
                    "GithubIssueCommentMention {{ mentioned_user: {} }}",
                    mentioned_user.as_deref().unwrap_or("None")
                )
            }
            TriggerType::GithubPullRequestReview => write!(f, "GithubPullRequestReview"),
            TriggerType::GithubPullRequestCommentMention { mentioned_user } => {
                write!(
                    f,
                    "GithubPullRequestCommentMention {{ mentioned_user: {} }}",
                    mentioned_user.as_deref().unwrap_or("None")
                )
            }
            TriggerType::GitlabIssueAssigned { assigned_to } => {
                write!(
                    f,
                    "GitlabIssueAssigned {{ assigned_to: {} }}",
                    assigned_to.as_deref().unwrap_or("None")
                )
            }
            TriggerType::GitlabIssueMention { mentioned_user } => {
                write!(
                    f,
                    "GitlabIssueMention {{ mentioned_user: {} }}",
                    mentioned_user.as_deref().unwrap_or("None")
                )
            }
            TriggerType::GitlabMergeRequestReview => write!(f, "GitlabMergeRequestReview"),
            TriggerType::GitlabMergeRequestCommentMention { mentioned_user } => {
                write!(
                    f,
                    "GitlabMergeRequestCommentMention {{ mentioned_user: {} }}",
                    mentioned_user.as_deref().unwrap_or("None")
                )
            }
        }
    }
}

impl TriggerType {
    /// Convert a `Trigger` struct into a typed `TriggerType` variant.
    ///
    /// Returns `None` if the trigger type string is not recognized.
    pub fn from_trigger(trigger: &Trigger) -> Option<Self> {
        match trigger.r#type.as_str() {
            triggers::GITHUB_ISSUE_ASSIGNED => Some(TriggerType::GithubIssueAssigned {
                assigned_to: trigger.assigned_to.clone(),
            }),
            triggers::GITHUB_ISSUE_COMMENT_MENTION => {
                Some(TriggerType::GithubIssueCommentMention {
                    mentioned_user: trigger.mentioned_user.clone(),
                })
            }
            triggers::GITHUB_PULL_REQUEST_REVIEW => Some(TriggerType::GithubPullRequestReview),
            triggers::GITHUB_PULL_REQUEST_COMMENT_MENTION => {
                Some(TriggerType::GithubPullRequestCommentMention {
                    mentioned_user: trigger.mentioned_user.clone(),
                })
            }
            triggers::GITLAB_ISSUE_ASSIGNED => Some(TriggerType::GitlabIssueAssigned {
                assigned_to: trigger.assigned_to.clone(),
            }),
            triggers::GITLAB_ISSUE_MENTION => Some(TriggerType::GitlabIssueMention {
                mentioned_user: trigger.mentioned_user.clone(),
            }),
            triggers::GITLAB_MERGE_REQUEST_REVIEW => Some(TriggerType::GitlabMergeRequestReview),
            triggers::GITLAB_MERGE_REQUEST_REVIEW_COMMENT => {
                Some(TriggerType::GitlabMergeRequestCommentMention {
                    mentioned_user: trigger.mentioned_user.clone(),
                })
            }
            _ => None,
        }
    }

    /// Return the string label for this trigger type (e.g. "github_issue_assigned").
    ///
    /// These labels match the `type` field values used in workflow TOML files
    /// and are the same strings parsed by `TriggerType::from_trigger()`.
    pub fn label(&self) -> &'static str {
        match self {
            TriggerType::GithubIssueAssigned { .. } => triggers::GITHUB_ISSUE_ASSIGNED,
            TriggerType::GithubIssueCommentMention { .. } => triggers::GITHUB_ISSUE_COMMENT_MENTION,
            TriggerType::GithubPullRequestReview => triggers::GITHUB_PULL_REQUEST_REVIEW,
            TriggerType::GithubPullRequestCommentMention { .. } => {
                triggers::GITHUB_PULL_REQUEST_COMMENT_MENTION
            }
            TriggerType::GitlabIssueAssigned { .. } => triggers::GITLAB_ISSUE_ASSIGNED,
            TriggerType::GitlabIssueMention { .. } => triggers::GITLAB_ISSUE_MENTION,
            TriggerType::GitlabMergeRequestReview => triggers::GITLAB_MERGE_REQUEST_REVIEW,
            TriggerType::GitlabMergeRequestCommentMention { .. } => {
                triggers::GITLAB_MERGE_REQUEST_REVIEW_COMMENT
            }
        }
    }

    /// Return the platform this trigger type belongs to.
    ///
    /// All current trigger types are platform-specific, so this always
    /// returns `Some`. The `Option` return type is preserved for
    /// forward-compatibility if platform-independent triggers are added later.
    pub fn platform(&self) -> Option<Platform> {
        match self {
            TriggerType::GithubIssueAssigned { .. }
            | TriggerType::GithubIssueCommentMention { .. }
            | TriggerType::GithubPullRequestReview
            | TriggerType::GithubPullRequestCommentMention { .. } => Some(Platform::Github),
            TriggerType::GitlabIssueAssigned { .. }
            | TriggerType::GitlabIssueMention { .. }
            | TriggerType::GitlabMergeRequestReview
            | TriggerType::GitlabMergeRequestCommentMention { .. } => Some(Platform::Gitlab),
        }
    }

    /// Return the platform-specific webhook event name for this trigger type.
    ///
    /// GitHub webhook events match the `X-GitHub-Event` header values.
    /// GitLab webhook events match the `X-GitLab-Event` header values
    /// but are expressed as boolean flags on the hook configuration.
    ///
    /// The returned strings are suitable for use in webhook subscription
    /// `events` arrays (GitHub) or as GitLab event flag names.
    pub fn webhook_event(&self) -> &'static str {
        match self {
            TriggerType::GithubIssueAssigned { .. } => crate::webhook::github::GITHUB_ISSUES,
            TriggerType::GithubIssueCommentMention { .. } => {
                crate::webhook::github::GITHUB_ISSUE_COMMENT
            }
            TriggerType::GithubPullRequestReview => {
                crate::webhook::github::GITHUB_PULL_REQUEST_REVIEW
            }
            TriggerType::GithubPullRequestCommentMention { .. } => {
                crate::webhook::github::GITHUB_PULL_REQUEST_REVIEW_COMMENT
            }
            TriggerType::GitlabIssueAssigned { .. } => "issues_events",
            TriggerType::GitlabIssueMention { .. } => "note_events",
            TriggerType::GitlabMergeRequestReview => "note_events",
            TriggerType::GitlabMergeRequestCommentMention { .. } => "note_events",
        }
    }

    /// Return the set of known template variables available at runtime for this trigger type.
    ///
    /// Global variables (`owner`, `repo`, `output_dir`, `event_id`, `repo_path`)
    /// are always included. Trigger-specific variables are added based on the
    /// platform and event type, matching the variables populated by the
    /// webhook handlers and dispatcher.
    pub fn known_variables(&self) -> std::collections::HashSet<String> {
        let mut vars = std::collections::HashSet::new();
        // Global variables available to all triggers
        vars.insert("owner".to_string());
        vars.insert("repo".to_string());
        vars.insert("output_dir".to_string());
        vars.insert("event_id".to_string());
        vars.insert("repo_path".to_string());

        match self {
            TriggerType::GithubIssueAssigned { .. } => {
                vars.insert("issue_number".to_string());
                vars.insert("assignee".to_string());
                vars.insert("issue_title".to_string());
                vars.insert("issue_body".to_string());
            }
            TriggerType::GithubIssueCommentMention { .. } => {
                vars.insert("issue_number".to_string());
                vars.insert("comment_id".to_string());
                vars.insert("comment_body".to_string());
                vars.insert("mentioned_user".to_string());
            }
            TriggerType::GithubPullRequestReview => {
                vars.insert("pr_number".to_string());
                vars.insert("review_id".to_string());
                vars.insert("review_body".to_string());
            }
            TriggerType::GithubPullRequestCommentMention { .. } => {
                vars.insert("pr_number".to_string());
                vars.insert("review_id".to_string());
                vars.insert("comment_id".to_string());
                vars.insert("comment_body".to_string());
                vars.insert("mentioned_user".to_string());
            }
            TriggerType::GitlabIssueAssigned { .. } => {
                vars.insert("issue_iid".to_string());
                vars.insert("issue_action".to_string());
            }
            TriggerType::GitlabIssueMention { .. } => {
                vars.insert("issue_iid".to_string());
                vars.insert("note_id".to_string());
                vars.insert("note_body".to_string());
            }
            TriggerType::GitlabMergeRequestReview => {
                vars.insert("mr_iid".to_string());
                vars.insert("note_id".to_string());
                vars.insert("note_body".to_string());
            }
            TriggerType::GitlabMergeRequestCommentMention { .. } => {
                vars.insert("mr_iid".to_string());
                vars.insert("note_id".to_string());
                vars.insert("note_body".to_string());
            }
        }
        vars
    }
}

/// Derive the set of unique webhook events required by a list of workflows.
///
/// Each workflow's trigger type is mapped to its corresponding platform
/// webhook event name via [`TriggerType::webhook_event`]. The result is
/// deduplicated (order-preserving) so that each event appears at most once.
///
/// Returns an empty `Vec` if no workflows have a recognized trigger type.
pub fn derive_required_events(workflows: &[Workflow]) -> Vec<String> {
    let mut events = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for wf in workflows {
        if let Some(tt) = TriggerType::from_trigger(&wf.trigger) {
            let event = tt.webhook_event().to_string();
            if seen.insert(event.clone()) {
                events.push(event);
            }
        }
    }
    events
}

impl Workflow {
    /// Validate the workflow meets semantic requirements:
    /// - trigger type must be non-empty and one of the known types
    /// - steps must not be empty
    /// - every step must have a non-empty prompt_template
    /// - all `{{variable}}` placeholders in `prompt_template` must be known
    ///   variables available at runtime for this workflow's trigger type
    /// - template syntax must be valid (no unclosed braces or empty placeholders)
    pub fn validate(&self) -> Result<(), String> {
        // Trigger type is required
        if self.trigger.r#type.is_empty() {
            return Err("trigger.type cannot be empty".to_string());
        }

        // Validate trigger type is a known value via TriggerType enum
        let trigger_type = TriggerType::from_trigger(&self.trigger)
            .ok_or_else(|| format!("invalid trigger type: {}", self.trigger.r#type))?;

        // allowed_users is a SECURITY BOUNDARY — it must be non-empty to prevent
        // prompt injection attacks from unauthorized users
        if self
            .trigger
            .allowed_users
            .as_ref()
            .is_none_or(|u| u.is_empty())
        {
            return Err(format!(
                "trigger.allowed_users must be defined and non-empty in workflow {}",
                self.path
            ));
        }

        // Steps array must not be empty
        if self.steps.is_empty() {
            return Err("workflow must contain at least one step".to_string());
        }

        // Build the set of known variables for this trigger type
        let known_vars = trigger_type.known_variables();

        // Each step must have a prompt_template with valid variable references
        for step in &self.steps {
            if step.prompt_template.trim().is_empty() {
                return Err(format!("step '{}' is missing prompt_template", step.name));
            }

            // Extract and validate template variables
            let vars = crate::template::extract_variables(&step.prompt_template)
                .map_err(|e| format!("syntax error in step '{}': {}", step.name, e))?;

            for var in vars {
                if !known_vars.contains(&var) {
                    return Err(format!(
                        "unknown template variable '{}' in step '{}' of workflow {}",
                        var, step.name, self.path
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Validate that all workflow trigger types match the configured platform.
///
/// Uses `TriggerType::from_trigger()` to parse each trigger and
/// `TriggerType::platform()` to check it matches the configured platform.
///
/// Returns `Ok(())` if all triggers match the platform, or `Err` with a clear
/// message identifying the mismatching workflow file and trigger type.
pub fn validate_triggers(
    platform: &Platform,
    workflows: &[(String, Workflow)],
) -> Result<(), String> {
    for (path, wf) in workflows {
        let Some(trigger_type) = TriggerType::from_trigger(&wf.trigger) else {
            // Unknown trigger types are caught by Workflow::validate() at load time.
            // If we reach here, something is very wrong.
            return Err(format!(
                "Workflow '{}' has unrecognized trigger type '{}'",
                path, wf.trigger.r#type
            ));
        };

        let trigger_platform = trigger_type.platform();
        if trigger_platform != Some(platform.clone()) {
            let platform_name = match platform {
                Platform::Github => "github",
                Platform::Gitlab => "gitlab",
            };
            return Err(format!(
                "Workflow '{}' has trigger '{}' but platform is '{}'",
                path, wf.trigger.r#type, platform_name
            ));
        }
    }
    Ok(())
}

/// Load all `.toml` workflow files from a directory, parsing and validating each.
///
/// Returns a list of `(file_path, Workflow)` pairs. The file path is preserved
/// for use in error messages during trigger platform validation.
pub fn load_workflows<P: AsRef<Path>>(dir: P) -> Result<Vec<(String, Workflow)>, WorkflowError> {
    let dir_ref = dir.as_ref();
    let dir_str = dir_ref.display().to_string();
    let mut workflows = Vec::new();
    for entry in fs::read_dir(dir_ref).map_err(|e| WorkflowError::Io {
        path: dir_str.clone(),
        source: e,
    })? {
        let entry = entry.map_err(|e| WorkflowError::Io {
            path: dir_str.clone(),
            source: e,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            let path_str = path.display().to_string();
            let content = fs::read_to_string(&path).map_err(|e| WorkflowError::Io {
                path: path_str.clone(),
                source: e,
            })?;
            let mut workflow: Workflow =
                toml::from_str(&content).map_err(|e| WorkflowError::Parse {
                    path: path_str.clone(),
                    source: e,
                })?;
            workflow.path = path_str.clone();
            workflow
                .validate()
                .map_err(|msg| WorkflowError::Validation {
                    path: path_str.clone(),
                    message: msg,
                })?;
            workflows.push((path_str, workflow));
        }
    }
    if workflows.is_empty() {
        return Err(WorkflowError::EmptyDirectory(
            "No workflow .toml files found".to_string(),
        ));
    }
    Ok(workflows)
}

/// Errors that can occur during workflow loading or validation.
#[derive(Debug)]
pub enum WorkflowError {
    /// I/O error reading the workflow directory.
    Io {
        /// The path being accessed when the error occurred.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// TOML parse or deserialize error.
    Parse {
        path: String,
        source: toml::de::Error,
    },
    /// Semantic validation error.
    Validation { path: String, message: String },
    /// No workflow `.toml` files found in the directory.
    EmptyDirectory(String),
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::Io { path, source } => {
                write!(f, "failed to read workflow path '{path}': {source}")
            }
            WorkflowError::Parse { path, source } => {
                write!(f, "parse error in {path}: {source}")
            }
            WorkflowError::Validation { path, message } => {
                write!(f, "validation error in {path}: {message}")
            }
            WorkflowError::EmptyDirectory(msg) => {
                write!(f, "empty workflow directory: {msg}")
            }
        }
    }
}

impl std::error::Error for WorkflowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkflowError::Io { source, .. } => Some(source),
            WorkflowError::Parse { source, .. } => Some(source),
            WorkflowError::Validation { .. } | WorkflowError::EmptyDirectory(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_TRIGGERS: &[&str] = &[
        triggers::GITHUB_ISSUE_ASSIGNED,
        triggers::GITHUB_ISSUE_COMMENT_MENTION,
        triggers::GITHUB_PULL_REQUEST_REVIEW,
        triggers::GITHUB_PULL_REQUEST_COMMENT_MENTION,
    ];

    const GITLAB_TRIGGERS: &[&str] = &[
        triggers::GITLAB_ISSUE_ASSIGNED,
        triggers::GITLAB_ISSUE_MENTION,
        triggers::GITLAB_MERGE_REQUEST_REVIEW,
        triggers::GITLAB_MERGE_REQUEST_REVIEW_COMMENT,
    ];

    #[test]
    fn test_valid_workflow_parse() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            assigned_to = "alice"

            [git]
            clone = true
            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan the issue"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(wf.trigger.r#type, triggers::GITHUB_ISSUE_ASSIGNED);
        assert_eq!(wf.trigger.assigned_to, Some("alice".to_string()));
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "Plan");
    }

    #[test]
    fn test_valid_gitlab_workflow_parse() {
        let toml = r#"
            [trigger]
            type = "gitlab_issue_assigned"
            allowed_users = ["testuser"]
            assigned_to = "alice"

            [git]
            clone = true
            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan the issue"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(wf.trigger.r#type, "gitlab_issue_assigned");
    }

    #[test]
    fn test_missing_trigger_type() {
        let toml = r#"
            [trigger]
            assigned_to = "alice"
            [git]
            clone = true

            default_branch = "main"
            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan"
        "#;
        // Missing `type` field should fail at serde deserialization
        assert!(toml::from_str::<Workflow>(toml).is_err());
    }

    #[test]
    fn test_invalid_trigger_type() {
        let toml = r#"
            [trigger]
            type = "unknown_event"
            [git]
            clone = true

            default_branch = "main"
            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_err());
    }

    #[test]
    fn test_empty_steps() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            [git]
            clone = true

            default_branch = "main"
            steps = []
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_err());
    }

    #[test]
    fn test_missing_prompt_template() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            [git]
            clone = true

            default_branch = "main"
            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = ""
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_err());
    }

    #[test]
    fn test_whitespace_only_prompt_template() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            [git]
            clone = true

            default_branch = "main"
            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "   "
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_err());
    }

    #[test]
    fn test_git_defaults() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Step"
            agent = "swe"
            prompt_template = "Do something"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        // When [git] section is omitted, defaults should apply (opt-in)
        assert!(!wf.git.clone);
        assert_eq!(wf.git.default_branch, "main");
    }

    #[test]
    fn test_git_clone_only() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [git]
            clone = true

            [[steps]]
            name = "Step"
            agent = "swe"
            prompt_template = "Do something"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert!(wf.git.clone);
        assert_eq!(wf.git.default_branch, "main");
    }

    #[test]
    fn test_git_worktree_rejected() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [git]
            clone = true
            worktree = true

            [[steps]]
            name = "Step"
            agent = "swe"
            prompt_template = "Do something"
        "#;
        let result = toml::from_str::<Workflow>(toml);
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("worktree"),
            "expected error to mention 'worktree', got: {msg}"
        );
        assert!(
            msg.contains("clone = true"),
            "expected error to suggest 'clone = true', got: {msg}"
        );
    }

    #[test]
    fn test_hook_deserialization() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            [git]
            clone = true

            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan it"
            pre_hooks = [{ type = "file_not_empty", path = "plan.md" }]
            post_hooks = [{ type = "file_contains", path = "plan.md", text = "implementation" }]
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(
            wf.steps[0].pre_hooks,
            vec![Hook::FileNotEmpty {
                path: "plan.md".to_string()
            }]
        );
        assert_eq!(
            wf.steps[0].post_hooks,
            vec![Hook::FileContains {
                path: "plan.md".to_string(),
                text: "implementation".to_string()
            }]
        );
    }

    #[test]
    fn test_multiple_steps() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]
            assigned_to = "alice"

            [git]
            clone = true

            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan the issue"

            [[steps]]
            name = "Implement"
            agent = "swe"
            prompt_template = "Implement the plan"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(wf.steps.len(), 2);
        assert_eq!(wf.steps[0].name, "Plan");
        assert_eq!(wf.steps[1].name, "Implement");
    }

    #[test]
    fn test_allowed_users() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["alice", "bob"]

            [git]
            clone = true

            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(
            wf.trigger.allowed_users,
            Some(vec!["alice".to_string(), "bob".to_string()])
        );
        assert!(wf.trigger.assigned_to.is_none());
    }

    // --- Template variable validation tests ---

    #[test]
    fn test_validate_known_variable_passes() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{owner}}/{{repo}}#{{issue_number}}"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
    }

    #[test]
    fn test_validate_unknown_variable_fails() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{typo_variable}}"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        let result = wf.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("unknown template variable"),
            "expected unknown variable error, got: {err}"
        );
        assert!(
            err.contains("typo_variable"),
            "error should mention the unknown variable name, got: {err}"
        );
        assert!(
            err.contains("Plan"),
            "error should mention the step name, got: {err}"
        );
    }

    #[test]
    fn test_validate_template_syntax_error() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        let result = wf.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("syntax error"),
            "expected syntax error, got: {err}"
        );
    }

    #[test]
    fn test_validate_empty_placeholder_fails() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{}}"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        let result = wf.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("syntax error"),
            "expected syntax error, got: {err}"
        );
    }

    #[test]
    fn test_validate_gitlab_known_variable_passes() {
        let toml = r#"
            [trigger]
            type = "gitlab_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{owner}}/{{repo}} issue {{issue_iid}}"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
    }

    #[test]
    fn test_validate_cross_platform_variable_fails() {
        // Using a GitHub variable (issue_number) in a GitLab workflow should fail
        let toml = r#"
            [trigger]
            type = "gitlab_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan {{issue_number}}"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        let result = wf.validate();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("unknown template variable"),
            "expected unknown variable error, got: {err}"
        );
    }

    #[test]
    fn test_validate_no_variables_passes() {
        // Templates without any {{}} are fine
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            allowed_users = ["testuser"]

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Just a plain template"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
    }

    #[test]
    fn test_load_workflows_from_directory() {
        let dir = std::env::temp_dir().join("yoke_test_workflows");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let valid_toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["testuser"]

[git]
clone = true

default_branch = "main"

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan it"
"#;
        fs::write(dir.join("valid.toml"), valid_toml).unwrap();

        // Write a non-toml file that should be skipped
        fs::write(dir.join("notes.txt"), "not a workflow").unwrap();

        let workflows = load_workflows(&dir).unwrap();
        assert_eq!(workflows.len(), 1);
        assert!(workflows[0].0.contains("valid.toml"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_load_workflows_invalid_toml() {
        let dir = std::env::temp_dir().join("yoke_test_invalid");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let invalid_toml = r#"
[trigger]
type = "invalid_trigger"

[git]
clone = true

default_branch = "main"

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan"
"#;
        fs::write(dir.join("bad.toml"), invalid_toml).unwrap();

        let result = load_workflows(&dir);
        assert!(result.is_err());
        match result.unwrap_err() {
            WorkflowError::Validation { path, message } => {
                assert!(path.contains("bad.toml"));
                assert!(message.contains("invalid trigger type"));
            }
            other => panic!("expected Validation error, got: {other}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_error_display() {
        let err = WorkflowError::Validation {
            path: "flows/bad.toml".to_string(),
            message: "invalid trigger type: foo".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("flows/bad.toml"));
        assert!(msg.contains("invalid trigger type: foo"));
    }

    #[test]
    fn test_io_error_includes_path() {
        let err = WorkflowError::Io {
            path: "workflows/bad.toml".to_string(),
            source: std::io::Error::other("test"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("workflows/bad.toml"),
            "Io error should include the path, got: {msg}"
        );
    }

    // --- Trigger platform validation tests ---

    fn make_workflow(trigger_type: &str) -> Workflow {
        let toml = format!(
            r#"
[trigger]
type = "{}"
allowed_users = ["testuser"]

[[steps]]
name = "Step"
agent = "swe"
prompt_template = "Do the thing"
"#,
            trigger_type
        );
        toml::from_str(&toml).unwrap()
    }

    #[test]
    fn test_validate_triggers_github_with_github_platform() {
        let platform = Platform::Github;
        let wf = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let workflows = vec![("workflows/plan.toml".to_string(), wf)];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_gitlab_with_gitlab_platform() {
        let platform = Platform::Gitlab;
        let wf = make_workflow(triggers::GITLAB_ISSUE_ASSIGNED);
        let workflows = vec![("workflows/plan.toml".to_string(), wf)];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_mismatch_gitlab_trigger_on_github() {
        let platform = Platform::Github;
        let wf = make_workflow(triggers::GITLAB_ISSUE_ASSIGNED);
        let workflows = vec![("workflows/gitlab-plan.toml".to_string(), wf)];
        let result = validate_triggers(&platform, &workflows);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("gitlab-plan.toml"),
            "error should contain workflow path, got: {err}"
        );
        assert!(
            err.contains(triggers::GITLAB_ISSUE_ASSIGNED),
            "error should contain trigger type, got: {err}"
        );
        assert!(
            err.contains("platform is 'github'"),
            "error should contain platform name, got: {err}"
        );
    }

    #[test]
    fn test_validate_triggers_mismatch_github_trigger_on_gitlab() {
        let platform = Platform::Gitlab;
        let wf = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let workflows = vec![("workflows/github-plan.toml".to_string(), wf)];
        let result = validate_triggers(&platform, &workflows);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("github-plan.toml"),
            "error should contain workflow path, got: {err}"
        );
        assert!(
            err.contains(triggers::GITHUB_ISSUE_ASSIGNED),
            "error should contain trigger type, got: {err}"
        );
        assert!(
            err.contains("platform is 'gitlab'"),
            "error should contain platform name, got: {err}"
        );
    }

    #[test]
    fn test_validate_triggers_multiple_workflows_all_valid() {
        let platform = Platform::Github;
        let wf1 = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let wf2 = make_workflow(triggers::GITHUB_PULL_REQUEST_REVIEW);
        let workflows = vec![
            ("workflows/issue.toml".to_string(), wf1),
            ("workflows/review.toml".to_string(), wf2),
        ];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_mixed_valid_and_invalid() {
        let platform = Platform::Github;
        let wf1 = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let wf2 = make_workflow(triggers::GITLAB_ISSUE_ASSIGNED);
        let workflows = vec![
            ("workflows/issue.toml".to_string(), wf1),
            ("workflows/gitlab-flow.toml".to_string(), wf2),
        ];
        let result = validate_triggers(&platform, &workflows);
        assert!(result.is_err());
        // Should fail on the gitlab trigger
        let err = result.unwrap_err();
        assert!(err.contains("gitlab-flow.toml"));
    }

    #[test]
    fn test_validate_triggers_all_gitlab_types() {
        let platform = Platform::Gitlab;
        for trigger_type in GITLAB_TRIGGERS {
            let wf = make_workflow(trigger_type);
            let workflows = vec![("workflows/test.toml".to_string(), wf)];
            assert!(
                validate_triggers(&platform, &workflows).is_ok(),
                "expected '{}' to pass validation on gitlab platform",
                trigger_type
            );
        }
    }

    #[test]
    fn test_validate_triggers_all_github_types() {
        let platform = Platform::Github;
        for trigger_type in GITHUB_TRIGGERS {
            let wf = make_workflow(trigger_type);
            let workflows = vec![("workflows/test.toml".to_string(), wf)];
            assert!(
                validate_triggers(&platform, &workflows).is_ok(),
                "expected '{}' to pass validation on github platform",
                trigger_type
            );
        }
    }

    #[test]
    fn test_validate_triggers_empty_workflows() {
        let platform = Platform::Github;
        let workflows: Vec<(String, Workflow)> = vec![];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    // --- TriggerType enum tests ---

    #[test]
    fn test_trigger_type_from_trigger_github() {
        let trigger = Trigger {
            r#type: triggers::GITHUB_ISSUE_ASSIGNED.to_string(),
            assigned_to: Some("alice".to_string()),
            mentioned_user: None,
            allowed_users: None,
        };
        let tt = TriggerType::from_trigger(&trigger).unwrap();
        assert_eq!(tt.platform(), Some(Platform::Github));
    }

    #[test]
    fn test_trigger_type_from_trigger_gitlab() {
        let trigger = Trigger {
            r#type: triggers::GITLAB_ISSUE_MENTION.to_string(),
            assigned_to: None,
            mentioned_user: Some("bob".to_string()),
            allowed_users: None,
        };
        let tt = TriggerType::from_trigger(&trigger).unwrap();
        assert_eq!(tt.platform(), Some(Platform::Gitlab));
    }

    #[test]
    fn test_trigger_type_from_trigger_unknown() {
        let trigger = Trigger {
            r#type: "unknown_event".to_string(),
            assigned_to: None,
            mentioned_user: None,
            allowed_users: None,
        };
        assert!(TriggerType::from_trigger(&trigger).is_none());
    }

    #[test]
    fn test_trigger_type_carries_filter_fields() {
        let trigger = Trigger {
            r#type: triggers::GITHUB_ISSUE_COMMENT_MENTION.to_string(),
            assigned_to: None,
            mentioned_user: Some("carol".to_string()),
            allowed_users: Some(vec!["alice".to_string(), "bob".to_string()]),
        };
        let tt = TriggerType::from_trigger(&trigger).unwrap();
        match tt {
            TriggerType::GithubIssueCommentMention { mentioned_user } => {
                assert_eq!(mentioned_user, Some("carol".to_string()));
            }
            _ => panic!("expected GithubIssueCommentMention variant"),
        }
        // allowed_users is now checked at the Trigger/workflow level, not TriggerType
    }

    // --- derive_required_events tests ---

    #[test]
    fn test_derive_required_events_single_github_workflow() {
        let wf = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let events = derive_required_events(&[wf]);
        assert_eq!(
            events,
            vec![crate::webhook::github::GITHUB_ISSUES.to_string()]
        );
    }

    #[test]
    fn test_derive_required_events_multiple_github_workflows() {
        let wf1 = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let wf2 = make_workflow(triggers::GITHUB_ISSUE_COMMENT_MENTION);
        let events = derive_required_events(&[wf1, wf2]);
        assert_eq!(
            events,
            vec![
                crate::webhook::github::GITHUB_ISSUES.to_string(),
                crate::webhook::github::GITHUB_ISSUE_COMMENT.to_string()
            ]
        );
    }

    #[test]
    fn test_derive_required_events_deduplicates() {
        let wf1 = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let wf2 = make_workflow(triggers::GITHUB_ISSUE_ASSIGNED);
        let events = derive_required_events(&[wf1, wf2]);
        assert_eq!(
            events,
            vec![crate::webhook::github::GITHUB_ISSUES.to_string()]
        );
    }

    #[test]
    fn test_derive_required_events_gitlab_workflows() {
        let wf1 = make_workflow(triggers::GITLAB_ISSUE_ASSIGNED);
        let wf2 = make_workflow(triggers::GITLAB_ISSUE_MENTION);
        let wf3 = make_workflow(triggers::GITLAB_MERGE_REQUEST_REVIEW);
        let events = derive_required_events(&[wf1, wf2, wf3]);
        // gitlab_issue_mention and gitlab_merge_request_review both map to note_events
        assert_eq!(events, vec!["issues_events", "note_events"]);
    }

    #[test]
    fn test_derive_required_events_gitlab_dedup_note_events() {
        let wf1 = make_workflow(triggers::GITLAB_ISSUE_MENTION);
        let wf2 = make_workflow(triggers::GITLAB_MERGE_REQUEST_REVIEW);
        let wf3 = make_workflow(triggers::GITLAB_MERGE_REQUEST_REVIEW_COMMENT);
        let events = derive_required_events(&[wf1, wf2, wf3]);
        // All three map to note_events — deduplicated
        assert_eq!(events, vec!["note_events"]);
    }

    #[test]
    fn test_derive_required_events_empty() {
        let events: Vec<String> = derive_required_events(&[]);
        assert!(events.is_empty());
    }

    // --- TriggerType::webhook_event tests ---

    #[test]
    fn test_webhook_event_github_triggers() {
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITHUB_ISSUE_ASSIGNED.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "issues"
        );
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITHUB_ISSUE_COMMENT_MENTION.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "issue_comment"
        );
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITHUB_PULL_REQUEST_REVIEW.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "pull_request_review"
        );
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITHUB_PULL_REQUEST_COMMENT_MENTION.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "pull_request_review_comment"
        );
    }

    #[test]
    fn test_webhook_event_gitlab_triggers() {
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITLAB_ISSUE_ASSIGNED.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "issues_events"
        );
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITLAB_ISSUE_MENTION.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "note_events"
        );
        assert_eq!(
            TriggerType::from_trigger(&Trigger {
                r#type: triggers::GITLAB_MERGE_REQUEST_REVIEW.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "note_events"
        );
    }

    // --- TriggerType::known_variables tests ---

    #[test]
    fn test_known_variables_github_issue_assigned() {
        let tt = TriggerType::GithubIssueAssigned { assigned_to: None };
        let vars = tt.known_variables();
        // Global
        assert!(vars.contains("owner"));
        assert!(vars.contains("repo"));
        assert!(vars.contains("output_dir"));
        assert!(vars.contains("event_id"));
        assert!(vars.contains("repo_path"));
        // Trigger-specific
        assert!(vars.contains("issue_number"));
        assert!(vars.contains("assignee"));
        assert!(vars.contains("issue_title"));
        assert!(vars.contains("issue_body"));
        // Should NOT contain variables from other triggers
        assert!(!vars.contains("pr_number"));
        assert!(!vars.contains("comment_id"));
    }

    #[test]
    fn test_known_variables_gitlab_issue_assigned() {
        let tt = TriggerType::GitlabIssueAssigned { assigned_to: None };
        let vars = tt.known_variables();
        // Global
        assert!(vars.contains("owner"));
        assert!(vars.contains("repo"));
        // Trigger-specific
        assert!(vars.contains("issue_iid"));
        assert!(vars.contains("issue_action"));
        // Should NOT contain GitHub variables
        assert!(!vars.contains("issue_number"));
    }

    #[test]
    fn test_known_variables_gitlab_merge_request_comment() {
        let tt = TriggerType::GitlabMergeRequestCommentMention {
            mentioned_user: None,
        };
        let vars = tt.known_variables();
        assert!(vars.contains("mr_iid"));
        assert!(vars.contains("note_id"));
        assert!(vars.contains("note_body"));
    }
}
