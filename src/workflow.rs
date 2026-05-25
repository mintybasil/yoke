#![allow(dead_code)] // Module will be wired into main in a follow-up task
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

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

/// Pre/post step hooks.
#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Hook {
    FileNotEmpty,
    FileContains,
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

        // Validate trigger type against known values
        let valid_triggers = ["github_issue_assigned", "manual"];
        if !valid_triggers.contains(&self.trigger.r#type.as_str()) {
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

/// Load all `.toml` workflow files from a directory, parsing and validating each.
pub fn load_workflows<P: AsRef<Path>>(dir: P) -> Result<Vec<Workflow>, WorkflowError> {
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
            workflow.path = path_str;
            workflow
                .validate()
                .map_err(|msg| WorkflowError::Validation {
                    path: path.display().to_string(),
                    message: msg,
                })?;
            workflows.push(workflow);
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
            type = "manual"

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
            pre_hooks = ["file_not_empty"]
            post_hooks = ["file_contains"]
        "#;
        let wf: Workflow = toml::from_str(toml).unwrap();
        assert!(wf.validate().is_ok());
        assert_eq!(wf.steps[0].pre_hooks, vec![Hook::FileNotEmpty]);
        assert_eq!(wf.steps[0].post_hooks, vec![Hook::FileContains]);
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
}
