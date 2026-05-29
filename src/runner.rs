//! Workflow runner: orchestrates sequential execution of multi-step workflows.
//!
//! The `WorkflowRunner` iterates through `Workflow::steps`, substituting variables
//! in `prompt_template` using the `template` module, validating state via the
//! `hooks` module, and invoking the `HermesClient` for agent execution.
//! A fail-fast error strategy is employed — the first error stops the workflow.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::harness::HermesClient;
use crate::workflow::Workflow;
use tracing::instrument;

/// Errors that can occur during workflow execution.
#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    /// Template rendering failed (unknown variable, malformed syntax, empty result).
    #[error("Template error: {0}")]
    Template(#[from] crate::template::TemplateError),
    /// Hook validation failed (file not found, empty, or missing text).
    #[error("Hook error: {0}")]
    Hook(#[from] crate::hooks::HookError),
    /// Hermes API call failed (network error, non-2xx response, parse error).
    #[error("Harness error: {0}")]
    Harness(#[from] crate::harness::HarnessError),
    /// A step failed during execution.
    #[error("Execution failed: {0}")]
    Execution(String),
}

/// Build the context-aware `instructions` string for the Hermes API.
///
/// When local file access is enabled (`git.clone` or `git.worktree` is true),
/// the instructions include the workspace directory path and an explicit `cd`
/// directive. When both are false (no local file access), a simple step name
/// is used instead.
fn build_instructions(workflow: &Workflow, workspace_dir: &Path, step_name: &str) -> String {
    if workflow.git.clone || workflow.git.worktree {
        let path = workspace_dir.to_string_lossy();
        format!(
            "All work is in: {}. Always run `cd {}` as your first action before any file or terminal operations. Reference all file paths relative to this directory.",
            path, path
        )
    } else {
        format!("Execute step: {}", step_name)
    }
}

/// Orchestrates execution of a `Workflow` within a specific workspace.
///
/// The runner iterates through each `Step` in the workflow, running pre-hooks,
/// rendering the prompt template, calling the Hermes API, and running post-hooks.
/// On the first error, execution stops (fail-fast).
pub struct WorkflowRunner {
    /// The workflow definition to execute.
    pub workflow: Workflow,
    /// Variables for `{{variable}}` template substitution.
    pub variables: HashMap<String, String>,
    /// The workspace directory where files are read/written (hooks, logs, etc.).
    pub workspace_dir: PathBuf,
    /// The Hermes API client for executing agent steps.
    pub client: HermesClient,
}

impl WorkflowRunner {
    /// Create a new `WorkflowRunner` with the given workflow, variables, workspace, and client.
    pub fn new(
        workflow: Workflow,
        variables: HashMap<String, String>,
        workspace_dir: PathBuf,
        client: HermesClient,
    ) -> Self {
        Self {
            workflow,
            variables,
            workspace_dir,
            client,
        }
    }

    /// Execute a single workflow step: pre-hooks → template render → API call → post-hooks.
    #[instrument(skip(self), fields(step = %step.name))]
    pub async fn execute_step(
        &self,
        step: &crate::workflow::Step,
    ) -> Result<crate::harness::StepResult, RunnerError> {
        // 1. Pre-hooks
        self.run_hooks(&step.pre_hooks)?;

        // 2. Render prompt template with variables
        let prompt = crate::template::render(&step.prompt_template, &self.variables)?;

        // 3. Build context-aware instructions based on git config
        let instructions = build_instructions(&self.workflow, &self.workspace_dir, &step.name);

        // 4. Call Hermes API
        let result = self.client.execute_step(&instructions, &prompt).await?;

        // 5. Post-hooks
        self.run_hooks(&step.post_hooks)?;

        Ok(result)
    }

    /// Run a list of hooks against the workspace directory.
    ///
    /// Each hook is executed in order. The first failing hook stops execution
    /// and returns `Err(RunnerError::Hook)`.
    fn run_hooks(&self, hooks: &[crate::workflow::Hook]) -> Result<(), RunnerError> {
        for hook in hooks {
            crate::hooks::run_hook(hook, self.workspace_dir.as_path())?;
        }
        Ok(())
    }

    /// Run the full workflow: execute all steps sequentially with fail-fast.
    ///
    /// Each step is executed in order via `execute_step`. On the first error,
    /// the workflow stops and returns `Err(RunnerError::Execution)`.
    /// On success, returns `Ok(())`.
    #[instrument(skip(self), fields(workflow = %self.workflow.path))]
    pub async fn run(&mut self) -> Result<(), RunnerError> {
        for step in &self.workflow.steps {
            self.execute_step(step).await.map_err(|e| {
                RunnerError::Execution(format!("Step '{}' failed: {}", step.name, e))
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::Hook;
    use crate::workflow::{GitConfig, Step, Trigger};

    /// Build a minimal `Workflow` for testing.
    fn test_workflow(steps: Vec<Step>) -> Workflow {
        Workflow {
            path: "test.toml".to_string(),
            trigger: Trigger {
                r#type: "github_issue_assigned".to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            },
            git: GitConfig::default(),
            steps,
        }
    }

    /// Build a `Workflow` with custom `GitConfig` for testing instructions.
    fn test_workflow_with_git(steps: Vec<Step>, git: GitConfig) -> Workflow {
        let mut workflow = test_workflow(steps);
        workflow.git = git;
        workflow
    }

    // --- Tests for build_instructions ---

    #[test]
    fn test_build_instructions_with_git_clone() {
        let workflow = test_workflow_with_git(
            vec![],
            GitConfig {
                clone: true,
                worktree: false,
                default_branch: "main".to_string(),
            },
        );
        let workspace_dir = PathBuf::from("/var/lib/yoke/mintybasil/yoke/42");
        let instructions = build_instructions(&workflow, &workspace_dir, "Plan");

        assert!(instructions.contains("/var/lib/yoke/mintybasil/yoke/42"));
        assert!(instructions.contains("cd /var/lib/yoke/mintybasil/yoke/42"));
        assert!(instructions.contains("All work is in:"));
        assert!(instructions.contains("Reference all file paths relative to this directory"));
    }

    #[test]
    fn test_build_instructions_with_git_worktree() {
        let workflow = test_workflow_with_git(
            vec![],
            GitConfig {
                clone: false,
                worktree: true,
                default_branch: "main".to_string(),
            },
        );
        let workspace_dir = PathBuf::from("/var/lib/yoke/mintybasil/yoke/42/worktree-1");
        let instructions = build_instructions(&workflow, &workspace_dir, "Implement");

        assert!(instructions.contains("/var/lib/yoke/mintybasil/yoke/42/worktree-1"));
        assert!(instructions.contains("cd /var/lib/yoke/mintybasil/yoke/42/worktree-1"));
    }

    #[test]
    fn test_build_instructions_with_both_git_enabled() {
        let workflow = test_workflow_with_git(
            vec![],
            GitConfig {
                clone: true,
                worktree: true,
                default_branch: "main".to_string(),
            },
        );
        let workspace_dir = PathBuf::from("/var/lib/yoke/org/repo/100");
        let instructions = build_instructions(&workflow, &workspace_dir, "Review");

        assert!(instructions.contains("/var/lib/yoke/org/repo/100"));
        assert!(instructions.contains("cd /var/lib/yoke/org/repo/100"));
    }

    #[test]
    fn test_build_instructions_without_git() {
        let workflow = test_workflow_with_git(
            vec![],
            GitConfig {
                clone: false,
                worktree: false,
                default_branch: "main".to_string(),
            },
        );
        let workspace_dir = PathBuf::from("/var/lib/yoke/org/repo/42");
        let instructions = build_instructions(&workflow, &workspace_dir, "Plan");

        assert_eq!(instructions, "Execute step: Plan");
        assert!(!instructions.contains("cd"));
        assert!(!instructions.contains("All work is in:"));
    }

    // --- Existing tests ---

    #[test]
    fn test_runner_error_display() {
        let err = RunnerError::Execution("something went wrong".to_string());
        assert_eq!(format!("{err}"), "Execution failed: something went wrong");

        let hook_err = crate::hooks::HookError::FileNotFound {
            path: "plan.md".to_string(),
        };
        let err = RunnerError::Hook(hook_err);
        assert!(format!("{err}").contains("plan.md"));
    }

    #[test]
    fn test_runner_new() {
        let workflow = test_workflow(vec![]);
        let variables = HashMap::new();
        let workspace_dir = PathBuf::from("/tmp/yoke-test");
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());

        let runner = WorkflowRunner::new(workflow, variables, workspace_dir.clone(), client);
        assert_eq!(runner.workflow.steps.len(), 0);
        assert_eq!(runner.workspace_dir, workspace_dir);
    }

    #[test]
    fn test_run_hooks_empty() {
        let workflow = test_workflow(vec![]);
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());
        let runner = WorkflowRunner::new(workflow, HashMap::new(), PathBuf::from("/tmp"), client);

        // Empty hooks list should succeed
        assert!(runner.run_hooks(&[]).is_ok());
    }

    #[test]
    fn test_run_hooks_file_not_empty_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plan.md"), "content").unwrap();

        let workflow = test_workflow(vec![]);
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());
        let runner =
            WorkflowRunner::new(workflow, HashMap::new(), dir.path().to_path_buf(), client);

        let hooks = vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }];
        assert!(runner.run_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_run_hooks_file_not_empty_fails() {
        let dir = tempfile::tempdir().unwrap();

        let workflow = test_workflow(vec![]);
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());
        let runner =
            WorkflowRunner::new(workflow, HashMap::new(), dir.path().to_path_buf(), client);

        let hooks = vec![Hook::FileNotEmpty {
            path: "missing.md".to_string(),
        }];
        let result = runner.run_hooks(&hooks);
        assert!(result.is_err());
        match result.unwrap_err() {
            RunnerError::Hook(_) => {}
            other => panic!("expected Hook error, got {other:?}"),
        }
    }

    #[test]
    fn test_run_hooks_file_contains_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("output.md"), "implementation plan").unwrap();

        let workflow = test_workflow(vec![]);
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());
        let runner =
            WorkflowRunner::new(workflow, HashMap::new(), dir.path().to_path_buf(), client);

        let hooks = vec![Hook::FileContains {
            path: "output.md".to_string(),
            text: "implementation".to_string(),
        }];
        assert!(runner.run_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_run_hooks_multiple_first_failure_stops() {
        let dir = tempfile::tempdir().unwrap();
        // Create file1 with content, but file2 doesn't exist
        std::fs::write(dir.path().join("file1.txt"), "content").unwrap();

        let workflow = test_workflow(vec![]);
        let client = HermesClient::new("http://localhost:8000".to_string(), "test-key".to_string());
        let runner =
            WorkflowRunner::new(workflow, HashMap::new(), dir.path().to_path_buf(), client);

        let hooks = vec![
            Hook::FileNotEmpty {
                path: "missing.txt".to_string(),
            },
            Hook::FileNotEmpty {
                path: "file1.txt".to_string(),
            },
        ];
        let result = runner.run_hooks(&hooks);
        assert!(result.is_err());
    }
}
