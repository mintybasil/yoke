//! Workflow runner: orchestrates sequential execution of multi-step workflows.
//!
//! The `WorkflowRunner` iterates through `Workflow::steps`, substituting variables
//! in `prompt_template` using the `template` module, validating state via the
//! `hooks` module, and invoking the `HermesClient` for agent execution.
//! A fail-fast error strategy is employed — the first error stops the workflow.
//!
//! Each step declares its own `agent` field. The runner resolves the correct
//! `AgentConfig` by name for each step and creates a `HermesClient` on-the-fly,
//! allowing different steps in a single workflow to target different Hermes
//! API instances.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::config::AgentConfig;
use crate::file_log;
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
    /// Agent referenced by a step was not found in the available configuration.
    #[error("Unknown agent '{agent}' referenced in step '{step}'")]
    UnknownAgent {
        /// The agent name that was not found.
        agent: String,
        /// The step that referenced the unknown agent.
        step: String,
    },
}

/// Build context-aware `instructions` for the Hermes API.
///
/// When local file access is enabled (`git.clone` or `git.worktree` is true),
/// returns `Some(instructions)` containing the workspace directory path and an
/// explicit `cd` directive. When both are false (no local file access), returns
/// `None` — the `instructions` field is omitted from the API request entirely,
/// since the step name is already passed as the prompt (`input`).
fn build_instructions(workflow: &Workflow, workspace_dir: &Path) -> Option<String> {
    if workflow.git.clone || workflow.git.worktree {
        let path = workspace_dir.to_string_lossy();
        Some(format!(
            "All work is in: {}. Always run `cd {}` as your first action before any file or terminal operations. Reference all file paths relative to this directory.",
            path, path
        ))
    } else {
        None
    }
}

/// Orchestrates execution of a `Workflow` within a specific workspace.
///
/// The runner iterates through each `Step` in the workflow, resolving the
/// agent for each step, running pre-hooks, rendering the prompt template,
/// calling the Hermes API, and running post-hooks.
/// On the first error, execution stops (fail-fast).
pub struct WorkflowRunner {
    /// The workflow definition to execute.
    pub workflow: Workflow,
    /// Variables for `{{variable}}` template substitution.
    pub variables: HashMap<String, String>,
    /// The workspace directory where files are read/written (hooks, logs, etc.).
    pub workspace_dir: PathBuf,
    /// Available agent configurations for per-step resolution.
    pub agents: Vec<AgentConfig>,
    /// The API key for authenticating with Hermes API instances.
    pub api_key: String,
    /// Step counter for file naming (0-based, incremented per step).
    step_counter: AtomicUsize,
}

impl WorkflowRunner {
    /// Create a new `WorkflowRunner` with the given workflow, variables, workspace,
    /// agent configurations, and API key.
    pub fn new(
        workflow: Workflow,
        variables: HashMap<String, String>,
        workspace_dir: PathBuf,
        agents: Vec<AgentConfig>,
        api_key: String,
    ) -> Self {
        Self {
            workflow,
            variables,
            workspace_dir,
            agents,
            api_key,
            step_counter: AtomicUsize::new(0),
        }
    }

    /// Resolve an `AgentConfig` by name from the available agents list.
    ///
    /// Returns `Err(RunnerError::UnknownAgent)` if no agent with the given name exists.
    fn resolve_agent(
        &self,
        agent_name: &str,
        step_name: &str,
    ) -> Result<&AgentConfig, RunnerError> {
        self.agents
            .iter()
            .find(|a| a.name == agent_name)
            .ok_or_else(|| RunnerError::UnknownAgent {
                agent: agent_name.to_string(),
                step: step_name.to_string(),
            })
    }

    /// Execute a single workflow step: resolve agent → pre-hooks → template render →
    /// write prompt file → log request → API call → log full exchange → post-hooks.
    #[instrument(skip(self), fields(step = %step.name, agent = %step.agent))]
    pub async fn execute_step(
        &self,
        step: &crate::workflow::Step,
    ) -> Result<crate::harness::StepResult, RunnerError> {
        // 0. Resolve the agent for this step and create a client
        let agent_config = self.resolve_agent(&step.agent, &step.name)?;
        let client = HermesClient::new(agent_config.base_url.to_string(), self.api_key.clone());

        // 1. Pre-hooks
        self.run_hooks(&step.pre_hooks)?;

        // 2. Render prompt template with variables
        let prompt = crate::template::render(&step.prompt_template, &self.variables)?;

        // 3. Build context-aware instructions based on git config
        let instructions = build_instructions(&self.workflow, &self.workspace_dir);

        // 4. Write the rendered prompt file before the API call
        let step_num = self.step_counter.fetch_add(1, Ordering::Relaxed);
        if let Err(e) =
            file_log::write_prompt_file(step_num, &step.name, &prompt, &self.workspace_dir)
        {
            tracing::warn!(step = %step.name, error = %e, "Failed to write prompt file");
        }

        // 5. Build and log the request body before the API call
        //    This ensures the request is logged even if the API call fails.
        let raw_request = client.build_request_body(instructions.as_deref(), &prompt);
        if let Err(e) = file_log::write_request_log_file(
            step_num,
            &step.name,
            &raw_request,
            &self.workspace_dir,
        ) {
            tracing::warn!(step = %step.name, error = %e, "Failed to write request log file");
        }

        // 6. Call Hermes API
        let result = client
            .execute_step(instructions.as_deref(), &prompt)
            .await?;

        // 7. Overwrite the log file with the full exchange (request + response + message)
        if let Err(e) = file_log::write_log_file(
            step_num,
            &step.name,
            &result.raw_request,
            &result.raw_response,
            &result.extracted_message,
            &self.workspace_dir,
        ) {
            tracing::warn!(step = %step.name, error = %e, "Failed to write log file");
        }

        // 8. Post-hooks
        self.run_hooks(&step.post_hooks)?;

        Ok(result)
    }

    /// Run a list of hooks against the workspace directory.
    ///
    /// Each hook's `path` field is rendered using the template engine (replacing
    /// `{{variable}}` placeholders with values from `self.variables`) before
    /// being resolved relative to the workspace directory. The `text` field in
    /// `FileContains` hooks is also rendered to allow template variables there.
    ///
    /// Each hook is executed in order. The first failing hook stops execution
    /// and returns `Err(RunnerError::Hook)`. A template rendering error
    /// (unknown variable, malformed syntax) returns `Err(RunnerError::Template)`.
    fn run_hooks(&self, hooks: &[crate::workflow::Hook]) -> Result<(), RunnerError> {
        for hook in hooks {
            let rendered_hook = match hook {
                crate::workflow::Hook::FileNotEmpty { path } => {
                    let rendered_path = crate::template::render(path, &self.variables)?;
                    crate::workflow::Hook::FileNotEmpty {
                        path: rendered_path,
                    }
                }
                crate::workflow::Hook::FileContains { path, text } => {
                    let rendered_path = crate::template::render(path, &self.variables)?;
                    let rendered_text = crate::template::render(text, &self.variables)?;
                    crate::workflow::Hook::FileContains {
                        path: rendered_path,
                        text: rendered_text,
                    }
                }
            };
            crate::hooks::run_hook(&rendered_hook, self.workspace_dir.as_path())?;
        }
        Ok(())
    }

    /// Run the full workflow: execute all steps sequentially with fail-fast.
    ///
    /// Each step is executed in order via `execute_step`. On the first error,
    /// the workflow stops and returns `Err(RunnerError::Execution)`.
    /// On success, returns `Ok(())`.
    #[instrument(skip(self), fields(workflow = %std::path::Path::new(&self.workflow.path).file_name().unwrap_or_default().to_string_lossy()))]
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
                r#type: crate::workflow::triggers::GITHUB_ISSUE_ASSIGNED.to_string(),
                assigned_to: None,
                mentioned_user: None,
                allowed_users: None,
            },
            git: GitConfig::default(),
            steps,
        }
    }

    fn test_agents() -> Vec<AgentConfig> {
        vec![AgentConfig {
            name: "pm".to_string(),
            base_url: url::Url::parse("http://localhost:8000").unwrap(),
        }]
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
        let instructions = build_instructions(&workflow, &workspace_dir);

        assert!(instructions.is_some());
        let instructions = instructions.unwrap();
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
        let instructions = build_instructions(&workflow, &workspace_dir);

        assert!(instructions.is_some());
        let instructions = instructions.unwrap();
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
        let instructions = build_instructions(&workflow, &workspace_dir);

        assert!(instructions.is_some());
        let instructions = instructions.unwrap();
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
        let instructions = build_instructions(&workflow, &workspace_dir);

        assert!(instructions.is_none());
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

        let unknown_agent_err = RunnerError::UnknownAgent {
            agent: "ghost".to_string(),
            step: "Plan".to_string(),
        };
        let msg = format!("{unknown_agent_err}");
        assert!(msg.contains("ghost"));
        assert!(msg.contains("Plan"));
    }

    #[test]
    fn test_runner_new() {
        let workflow = test_workflow(vec![]);
        let variables = HashMap::new();
        let workspace_dir = PathBuf::from("/tmp/yoke-test");
        let agents = test_agents();
        let api_key = "test-key".to_string();

        let runner =
            WorkflowRunner::new(workflow, variables, workspace_dir.clone(), agents, api_key);
        assert_eq!(runner.workflow.steps.len(), 0);
        assert_eq!(runner.workspace_dir, workspace_dir);
        assert_eq!(runner.agents.len(), 1);
        assert_eq!(runner.api_key, "test-key");
    }

    #[test]
    fn test_resolve_agent_found() {
        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            PathBuf::from("/tmp"),
            agents,
            "key".to_string(),
        );

        let result = runner.resolve_agent("pm", "Plan");
        assert!(result.is_ok());
        assert_eq!(result.unwrap().name, "pm");
    }

    #[test]
    fn test_resolve_agent_not_found() {
        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            PathBuf::from("/tmp"),
            agents,
            "key".to_string(),
        );

        let result = runner.resolve_agent("ghost", "Plan");
        assert!(result.is_err());
        match result.unwrap_err() {
            RunnerError::UnknownAgent { agent, step } => {
                assert_eq!(agent, "ghost");
                assert_eq!(step, "Plan");
            }
            other => panic!("expected UnknownAgent error, got {other:?}"),
        }
    }

    #[test]
    fn test_run_hooks_empty() {
        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            PathBuf::from("/tmp"),
            agents,
            "key".to_string(),
        );

        // Empty hooks list should succeed
        assert!(runner.run_hooks(&[]).is_ok());
    }

    #[test]
    fn test_run_hooks_file_not_empty_passes() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plan.md"), "content").unwrap();

        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

        let hooks = vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }];
        assert!(runner.run_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_run_hooks_file_not_empty_fails() {
        let dir = tempfile::tempdir().unwrap();

        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

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
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

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
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

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

    #[test]
    fn test_run_hooks_templated_path() {
        let dir = tempfile::tempdir().unwrap();
        let workspace_path = dir.path().join("output/plan.md");
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        std::fs::write(&workspace_path, "content").unwrap();

        let workflow = test_workflow(vec![]);
        let agents = test_agents();

        let mut variables = HashMap::new();
        variables.insert("output_dir".to_string(), "output".to_string());

        let runner = WorkflowRunner::new(
            workflow,
            variables,
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

        let hooks = vec![Hook::FileNotEmpty {
            path: "{{output_dir}}/plan.md".to_string(),
        }];

        // This should now succeed — the template variable is rendered before the hook runs
        let result = runner.run_hooks(&hooks);
        assert!(
            result.is_ok(),
            "Templated hook path should resolve and find the file"
        );
    }

    #[test]
    fn test_run_hooks_templated_path_file_contains() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("output")).unwrap();
        std::fs::write(dir.path().join("output/result.md"), "implementation plan").unwrap();

        let workflow = test_workflow(vec![]);
        let agents = test_agents();

        let mut variables = HashMap::new();
        variables.insert("output_dir".to_string(), "output".to_string());
        variables.insert("keyword".to_string(), "implementation".to_string());

        let runner = WorkflowRunner::new(
            workflow,
            variables,
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

        let hooks = vec![Hook::FileContains {
            path: "{{output_dir}}/result.md".to_string(),
            text: "{{keyword}}".to_string(),
        }];

        let result = runner.run_hooks(&hooks);
        assert!(
            result.is_ok(),
            "Templated hook path and text should resolve correctly"
        );
    }

    #[test]
    fn test_run_hooks_plain_path_still_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("plan.md"), "content").unwrap();

        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

        let hooks = vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }];
        assert!(runner.run_hooks(&hooks).is_ok());
    }

    #[test]
    fn test_run_hooks_unknown_variable_fails() {
        let dir = tempfile::tempdir().unwrap();
        let workflow = test_workflow(vec![]);
        let agents = test_agents();
        let runner = WorkflowRunner::new(
            workflow,
            HashMap::new(),
            dir.path().to_path_buf(),
            agents,
            "key".to_string(),
        );

        let hooks = vec![Hook::FileNotEmpty {
            path: "{{unknown_var}}/plan.md".to_string(),
        }];
        let result = runner.run_hooks(&hooks);
        assert!(result.is_err());
        match result.unwrap_err() {
            RunnerError::Template(crate::template::TemplateError::UnknownVariable { name }) => {
                assert_eq!(name, "unknown_var");
            }
            other => panic!("expected TemplateError::UnknownVariable, got {other:?}"),
        }
    }
}
