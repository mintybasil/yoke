use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::Platform;

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

/// Git configuration for cloning and worktree management.
#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GitConfig {
    #[serde(default = "default_true")]
    pub clone: bool,
    #[serde(default = "default_true")]
    pub worktree: bool,
    #[serde(default = "default_branch")]
    pub default_branch: String,
}

fn default_true() -> bool {
    true
}

fn default_branch() -> String {
    "main".to_string()
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            clone: default_true(),
            worktree: default_true(),
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
    GithubIssueAssigned {
        assigned_to: Option<String>,
        allowed_users: Option<Vec<String>>,
    },
    GithubIssueCommentMention {
        mentioned_user: Option<String>,
        allowed_users: Option<Vec<String>>,
    },
    GithubPullRequestReview {
        allowed_users: Option<Vec<String>>,
    },
    GithubPullRequestCommentMention {
        mentioned_user: Option<String>,
        allowed_users: Option<Vec<String>>,
    },
    // GitLab triggers
    GitlabIssueAssigned {
        assigned_to: Option<String>,
    },
    GitlabIssueMention {
        mentioned_user: Option<String>,
        allowed_users: Option<Vec<String>>,
    },
    GitlabMergeRequestReview {
        allowed_users: Option<Vec<String>>,
    },
    GitlabMergeRequestCommentMention {
        mentioned_user: Option<String>,
        allowed_users: Option<Vec<String>>,
    },
}

impl TriggerType {
    /// Convert a `Trigger` struct into a typed `TriggerType` variant.
    ///
    /// Returns `None` if the trigger type string is not recognized.
    pub fn from_trigger(trigger: &Trigger) -> Option<Self> {
        match trigger.r#type.as_str() {
            "github_issue_assigned" => Some(TriggerType::GithubIssueAssigned {
                assigned_to: trigger.assigned_to.clone(),
                allowed_users: trigger.allowed_users.clone(),
            }),
            "github_issue_comment_mention" => Some(TriggerType::GithubIssueCommentMention {
                mentioned_user: trigger.mentioned_user.clone(),
                allowed_users: trigger.allowed_users.clone(),
            }),
            "github_pull_request_review" => Some(TriggerType::GithubPullRequestReview {
                allowed_users: trigger.allowed_users.clone(),
            }),
            "github_pull_request_review_comment" => {
                Some(TriggerType::GithubPullRequestCommentMention {
                    mentioned_user: trigger.mentioned_user.clone(),
                    allowed_users: trigger.allowed_users.clone(),
                })
            }
            "gitlab_issue_assigned" => Some(TriggerType::GitlabIssueAssigned {
                assigned_to: trigger.assigned_to.clone(),
            }),
            "gitlab_issue_mention" => Some(TriggerType::GitlabIssueMention {
                mentioned_user: trigger.mentioned_user.clone(),
                allowed_users: trigger.allowed_users.clone(),
            }),
            "gitlab_merge_request_review" => Some(TriggerType::GitlabMergeRequestReview {
                allowed_users: trigger.allowed_users.clone(),
            }),
            "gitlab_merge_request_review_comment" => {
                Some(TriggerType::GitlabMergeRequestCommentMention {
                    mentioned_user: trigger.mentioned_user.clone(),
                    allowed_users: trigger.allowed_users.clone(),
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
            TriggerType::GithubIssueAssigned { .. } => "github_issue_assigned",
            TriggerType::GithubIssueCommentMention { .. } => "github_issue_comment_mention",
            TriggerType::GithubPullRequestReview { .. } => "github_pull_request_review",
            TriggerType::GithubPullRequestCommentMention { .. } => {
                "github_pull_request_review_comment"
            }
            TriggerType::GitlabIssueAssigned { .. } => "gitlab_issue_assigned",
            TriggerType::GitlabIssueMention { .. } => "gitlab_issue_mention",
            TriggerType::GitlabMergeRequestReview { .. } => "gitlab_merge_request_review",
            TriggerType::GitlabMergeRequestCommentMention { .. } => {
                "gitlab_merge_request_review_comment"
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
            | TriggerType::GithubPullRequestReview { .. }
            | TriggerType::GithubPullRequestCommentMention { .. } => Some(Platform::Github),
            TriggerType::GitlabIssueAssigned { .. }
            | TriggerType::GitlabIssueMention { .. }
            | TriggerType::GitlabMergeRequestReview { .. }
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
            TriggerType::GithubIssueAssigned { .. } => "issues",
            TriggerType::GithubIssueCommentMention { .. } => "issue_comment",
            TriggerType::GithubPullRequestReview { .. } => "pull_request_review",
            TriggerType::GithubPullRequestCommentMention { .. } => "pull_request_review_comment",
            TriggerType::GitlabIssueAssigned { .. } => "issues_events",
            TriggerType::GitlabIssueMention { .. } => "note_events",
            TriggerType::GitlabMergeRequestReview { .. } => "note_events",
            TriggerType::GitlabMergeRequestCommentMention { .. } => "note_events",
        }
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
    pub fn validate(&self) -> Result<(), String> {
        // Trigger type is required
        if self.trigger.r#type.is_empty() {
            return Err("trigger.type cannot be empty".to_string());
        }

        // Validate trigger type is a known value via TriggerType enum
        if TriggerType::from_trigger(&self.trigger).is_none() {
            return Err(format!("invalid trigger type: {}", self.trigger.r#type));
        }

        // Steps array must not be empty
        if self.steps.is_empty() {
            return Err("workflow must contain at least one step".to_string());
        }

        // Each step must have a prompt_template
        for step in &self.steps {
            if step.prompt_template.trim().is_empty() {
                return Err(format!("step '{}' is missing prompt_template", step.name));
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
    let mut workflows = Vec::new();
    for entry in fs::read_dir(dir).map_err(WorkflowError::Io)? {
        let entry = entry.map_err(WorkflowError::Io)?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("toml") {
            let path_str = path.display().to_string();
            let content = fs::read_to_string(&path).map_err(WorkflowError::Io)?;
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
    Ok(workflows)
}

/// Errors that can occur during workflow loading or validation.
#[derive(Debug)]
pub enum WorkflowError {
    /// I/O error reading a file or directory.
    Io(std::io::Error),
    /// TOML parse or deserialize error.
    Parse {
        path: String,
        source: toml::de::Error,
    },
    /// Semantic validation error.
    Validation { path: String, message: String },
}

impl std::fmt::Display for WorkflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkflowError::Io(e) => write!(f, "I/O error: {e}"),
            WorkflowError::Parse { path, source } => {
                write!(f, "parse error in {path}: {source}")
            }
            WorkflowError::Validation { path, message } => {
                write!(f, "validation error in {path}: {message}")
            }
        }
    }
}

impl std::error::Error for WorkflowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            WorkflowError::Io(e) => Some(e),
            WorkflowError::Parse { source, .. } => Some(source),
            WorkflowError::Validation { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_TRIGGERS: &[&str] = &[
        "github_issue_assigned",
        "github_issue_comment_mention",
        "github_pull_request_review",
        "github_pull_request_review_comment",
    ];

    const GITLAB_TRIGGERS: &[&str] = &[
        "gitlab_issue_assigned",
        "gitlab_issue_mention",
        "gitlab_merge_request_review",
        "gitlab_merge_request_review_comment",
    ];

    #[test]
    fn test_valid_workflow_parse() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            assigned_to = "alice"

            [git]
            clone = true
            worktree = true
            default_branch = "main"

            [[steps]]
            name = "Plan"
            agent = "pm"
            prompt_template = "Plan the issue"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(wf.trigger.r#type, "github_issue_assigned");
        assert_eq!(wf.trigger.assigned_to, Some("alice".to_string()));
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.steps[0].name, "Plan");
    }

    #[test]
    fn test_valid_gitlab_workflow_parse() {
        let toml = r#"
            [trigger]
            type = "gitlab_issue_assigned"
            assigned_to = "alice"

            [git]
            clone = true
            worktree = true
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
            worktree = true
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
            worktree = true
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
            [git]
            clone = true
            worktree = true
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
            [git]
            clone = true
            worktree = true
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
            [git]
            clone = true
            worktree = true
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

            [[steps]]
            name = "Step"
            agent = "swe"
            prompt_template = "Do something"
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        // When [git] section is omitted, defaults should apply
        assert!(wf.git.clone);
        assert!(wf.git.worktree);
        assert_eq!(wf.git.default_branch, "main");
    }

    #[test]
    fn test_hook_deserialization() {
        let toml = r#"
            [trigger]
            type = "github_issue_assigned"
            [git]
            clone = true
            worktree = true
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
            assigned_to = "alice"

            [git]
            clone = true
            worktree = true
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
            worktree = true
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

    #[test]
    fn test_load_workflows_from_directory() {
        let dir = std::env::temp_dir().join("yoke_test_workflows");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let valid_toml = r#"
[trigger]
type = "github_issue_assigned"

[git]
clone = true
worktree = true
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
worktree = true
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

    // --- Trigger platform validation tests ---

    fn make_workflow(trigger_type: &str) -> Workflow {
        let toml = format!(
            r#"
[trigger]
type = "{}"

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
        let wf = make_workflow("github_issue_assigned");
        let workflows = vec![("workflows/plan.toml".to_string(), wf)];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_gitlab_with_gitlab_platform() {
        let platform = Platform::Gitlab;
        let wf = make_workflow("gitlab_issue_assigned");
        let workflows = vec![("workflows/plan.toml".to_string(), wf)];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_mismatch_gitlab_trigger_on_github() {
        let platform = Platform::Github;
        let wf = make_workflow("gitlab_issue_assigned");
        let workflows = vec![("workflows/gitlab-plan.toml".to_string(), wf)];
        let result = validate_triggers(&platform, &workflows);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("gitlab-plan.toml"),
            "error should contain workflow path, got: {err}"
        );
        assert!(
            err.contains("gitlab_issue_assigned"),
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
        let wf = make_workflow("github_issue_assigned");
        let workflows = vec![("workflows/github-plan.toml".to_string(), wf)];
        let result = validate_triggers(&platform, &workflows);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("github-plan.toml"),
            "error should contain workflow path, got: {err}"
        );
        assert!(
            err.contains("github_issue_assigned"),
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
        let wf1 = make_workflow("github_issue_assigned");
        let wf2 = make_workflow("github_pull_request_review");
        let workflows = vec![
            ("workflows/issue.toml".to_string(), wf1),
            ("workflows/review.toml".to_string(), wf2),
        ];
        assert!(validate_triggers(&platform, &workflows).is_ok());
    }

    #[test]
    fn test_validate_triggers_mixed_valid_and_invalid() {
        let platform = Platform::Github;
        let wf1 = make_workflow("github_issue_assigned");
        let wf2 = make_workflow("gitlab_issue_assigned");
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
            r#type: "github_issue_assigned".to_string(),
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
            r#type: "gitlab_issue_mention".to_string(),
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
            r#type: "github_issue_comment_mention".to_string(),
            assigned_to: None,
            mentioned_user: Some("carol".to_string()),
            allowed_users: Some(vec!["alice".to_string(), "bob".to_string()]),
        };
        let tt = TriggerType::from_trigger(&trigger).unwrap();
        match tt {
            TriggerType::GithubIssueCommentMention {
                mentioned_user,
                allowed_users,
            } => {
                assert_eq!(mentioned_user, Some("carol".to_string()));
                assert_eq!(
                    allowed_users,
                    Some(vec!["alice".to_string(), "bob".to_string()])
                );
            }
            _ => panic!("expected GithubIssueCommentMention variant"),
        }
    }

    // --- derive_required_events tests ---

    #[test]
    fn test_derive_required_events_single_github_workflow() {
        let wf = make_workflow("github_issue_assigned");
        let events = derive_required_events(&[wf]);
        assert_eq!(events, vec!["issues"]);
    }

    #[test]
    fn test_derive_required_events_multiple_github_workflows() {
        let wf1 = make_workflow("github_issue_assigned");
        let wf2 = make_workflow("github_issue_comment_mention");
        let events = derive_required_events(&[wf1, wf2]);
        assert_eq!(events, vec!["issues", "issue_comment"]);
    }

    #[test]
    fn test_derive_required_events_deduplicates() {
        let wf1 = make_workflow("github_issue_assigned");
        let wf2 = make_workflow("github_issue_assigned");
        let events = derive_required_events(&[wf1, wf2]);
        assert_eq!(events, vec!["issues"]);
    }

    #[test]
    fn test_derive_required_events_gitlab_workflows() {
        let wf1 = make_workflow("gitlab_issue_assigned");
        let wf2 = make_workflow("gitlab_issue_mention");
        let wf3 = make_workflow("gitlab_merge_request_review");
        let events = derive_required_events(&[wf1, wf2, wf3]);
        // gitlab_issue_mention and gitlab_merge_request_review both map to note_events
        assert_eq!(events, vec!["issues_events", "note_events"]);
    }

    #[test]
    fn test_derive_required_events_gitlab_dedup_note_events() {
        let wf1 = make_workflow("gitlab_issue_mention");
        let wf2 = make_workflow("gitlab_merge_request_review");
        let wf3 = make_workflow("gitlab_merge_request_review_comment");
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
                r#type: "github_issue_assigned".to_string(),
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
                r#type: "github_issue_comment_mention".to_string(),
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
                r#type: "github_pull_request_review".to_string(),
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
                r#type: "github_pull_request_review_comment".to_string(),
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
                r#type: "gitlab_issue_assigned".to_string(),
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
                r#type: "gitlab_issue_mention".to_string(),
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
                r#type: "gitlab_merge_request_review".to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            })
            .unwrap()
            .webhook_event(),
            "note_events"
        );
    }
}
