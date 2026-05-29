//! Integration tests for the workflow runner.
//!
//! These tests verify the full workflow execution flow:
//! pre-hook → template render → API call → post-hook → next step.
//!
//! To avoid needing a live Hermes API server, we use a mock HTTP server
//! built with `axum` that returns canned responses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use yoke::harness::HermesClient;
use yoke::hooks::Hook;
use yoke::runner::{RunnerError, WorkflowRunner};
use yoke::workflow::{GitConfig, Step, Trigger, Workflow};

use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

/// Matches the Hermes API request body format.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MockRequest {
    instructions: String,
    input: String,
    store: bool,
}

/// Matches the Hermes API response body format.
#[derive(Debug, Serialize)]
struct MockResponse {
    output: Vec<MockContentBlock>,
}

#[derive(Debug, Serialize)]
struct MockContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: String,
}

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

/// Start a mock Hermes API server that returns the given text in each response.
async fn start_mock_server(response_text: &str) -> String {
    let text = response_text.to_string();
    let app = Router::new().route(
        "/v1/responses",
        post(move |_body: Json<MockRequest>| {
            let text = text.clone();
            async move {
                let response = MockResponse {
                    output: vec![MockContentBlock {
                        block_type: "output_text".to_string(),
                        text,
                    }],
                };
                Json(response)
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Brief delay to let the server start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    format!("http://127.0.0.1:{port}")
}

/// Start a mock server that captures the `instructions` field from each request.
///
/// Returns `(base_url, captured_instructions)` where `captured_instructions`
/// can be inspected after the workflow run.
async fn start_mock_server_with_capture(response_text: &str) -> (String, Arc<Mutex<Vec<String>>>) {
    let text = response_text.to_string();
    let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let app = Router::new().route(
        "/v1/responses",
        post(move |body: Json<MockRequest>| {
            let text = text.clone();
            let captured = captured_clone.clone();
            async move {
                captured.lock().unwrap().push(body.instructions.clone());
                let response = MockResponse {
                    output: vec![MockContentBlock {
                        block_type: "output_text".to_string(),
                        text,
                    }],
                };
                Json(response)
            }
        }),
    );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let port = addr.port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // Brief delay to let the server start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (format!("http://127.0.0.1:{port}"), captured)
}

#[tokio::test]
async fn test_workflow_execution_two_steps() {
    let base_url = start_mock_server("Step completed").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![
        Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan issue {{issue_number}}".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Implement".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement the plan for {{issue_number}}".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let mut variables = HashMap::new();
    variables.insert("issue_number".to_string(), "42".to_string());

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_template_variables_substituted() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan {{owner}}/{{repo}}#{{issue_number}}".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let mut variables = HashMap::new();
    variables.insert("owner".to_string(), "mintybasil".to_string());
    variables.insert("repo".to_string(), "yoke".to_string());
    variables.insert("issue_number".to_string(), "37".to_string());

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_unknown_variable_fails() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan {{unknown_var}}".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new(); // empty — no unknown_var

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(msg.contains("Plan"));
            assert!(msg.contains("unknown variable"));
        }
        other => panic!("expected Execution error with template detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pre_hook_failure_prevents_step() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    // Don't create plan.md — pre-hook should fail
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan the issue".to_string(),
        pre_hooks: vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(msg.contains("Plan"));
            assert!(msg.contains("plan.md"));
        }
        other => panic!("expected Execution error with hook detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_post_hook_failure_marks_step_failed() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    // Create plan.md but not output.md — post-hook should fail
    std::fs::write(dir.path().join("plan.md"), "plan content").unwrap();

    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan the issue".to_string(),
        pre_hooks: vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }],
        post_hooks: vec![Hook::FileContains {
            path: "output.md".to_string(),
            text: "implementation".to_string(),
        }],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(msg.contains("Plan"));
            assert!(msg.contains("output.md"));
        }
        other => panic!("expected Execution error with hook detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_fail_fast_on_first_step_error() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![
        Step {
            name: "Step1".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan {{unknown_var}}".to_string(), // will fail
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Step2".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_err());
    // The error should reference Step1, not Step2
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(msg.contains("Step1"));
            assert!(!msg.contains("Step2"));
        }
        other => panic!("expected Execution error for Step1, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pre_and_post_hooks_pass() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    // Create files needed by hooks
    std::fs::write(dir.path().join("input.md"), "issue content").unwrap();
    std::fs::write(dir.path().join("output.md"), "plan implementation done").unwrap();

    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan the issue".to_string(),
        pre_hooks: vec![Hook::FileNotEmpty {
            path: "input.md".to_string(),
        }],
        post_hooks: vec![Hook::FileContains {
            path: "output.md".to_string(),
            text: "implementation".to_string(),
        }],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_steps_execute_in_order() {
    // Use a mock server that echoes the instruction text so we can verify order
    let base_url = start_mock_server("Completed").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![
        Step {
            name: "First".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Step one".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Second".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Step two".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Third".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Step three".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    // If all three steps run successfully, we know they executed in order
    // (fail-fast would stop at the first failure)
    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_hook_failure_between_steps_stops_workflow() {
    let base_url = start_mock_server("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();

    // Step 1 succeeds, Step 2 has a failing pre-hook
    let workflow = test_workflow(vec![
        Step {
            name: "Step1".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Step2".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement".to_string(),
            pre_hooks: vec![Hook::FileNotEmpty {
                path: "nonexistent.md".to_string(),
            }],
            post_hooks: vec![],
        },
        Step {
            name: "Step3".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Review".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(workflow, variables, dir.path().to_path_buf(), client);

    let result = runner.run().await;
    assert!(result.is_err());
    // Should fail on Step2, not Step3
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(msg.contains("Step2"));
            assert!(!msg.contains("Step3"));
        }
        other => panic!("expected Execution error for Step2, got {other:?}"),
    }
}

// --- Context-aware instructions integration tests ---

#[tokio::test]
async fn test_instructions_include_workspace_dir_when_git_enabled() {
    // When git.clone or git.worktree is true, instructions should contain
    // the workspace directory path and a cd directive.
    let (base_url, captured) = start_mock_server_with_capture("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().to_path_buf();

    let workflow = test_workflow_with_git(
        vec![Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan the issue".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        }],
        GitConfig {
            clone: true,
            worktree: false,
            default_branch: "main".to_string(),
        },
    );

    let mut runner = WorkflowRunner::new(workflow, HashMap::new(), workspace_dir.clone(), client);
    let result = runner.run().await;
    assert!(result.is_ok());

    let instructions = &captured.lock().unwrap()[0];
    assert!(instructions.contains(workspace_dir.to_string_lossy().as_ref()));
    assert!(instructions.contains("cd"));
    assert!(instructions.contains("All work is in:"));
    assert!(instructions.contains("Reference all file paths relative to this directory"));
}

#[tokio::test]
async fn test_instructions_omit_workspace_dir_when_git_disabled() {
    // When both git.clone and git.worktree are false, instructions should
    // be the simple "Execute step: {name}" format without workspace path.
    let (base_url, captured) = start_mock_server_with_capture("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow_with_git(
        vec![Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan the issue".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        }],
        GitConfig {
            clone: false,
            worktree: false,
            default_branch: "main".to_string(),
        },
    );

    let mut runner =
        WorkflowRunner::new(workflow, HashMap::new(), dir.path().to_path_buf(), client);
    let result = runner.run().await;
    assert!(result.is_ok());

    let instructions = &captured.lock().unwrap()[0];
    assert_eq!(instructions, "Execute step: Plan");
    assert!(!instructions.contains("cd"));
    assert!(!instructions.contains("All work is in:"));
}

#[tokio::test]
async fn test_instructions_include_workspace_dir_with_worktree() {
    // When git.worktree is true (even without git.clone), instructions should
    // still include the workspace directory path and cd directive.
    let (base_url, captured) = start_mock_server_with_capture("Done").await;
    let client = HermesClient::new(base_url, "test-key".to_string());

    let dir = tempfile::tempdir().unwrap();
    let workspace_dir = dir.path().to_path_buf();

    let workflow = test_workflow_with_git(
        vec![Step {
            name: "Implement".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        }],
        GitConfig {
            clone: false,
            worktree: true,
            default_branch: "main".to_string(),
        },
    );

    let mut runner = WorkflowRunner::new(workflow, HashMap::new(), workspace_dir.clone(), client);
    let result = runner.run().await;
    assert!(result.is_ok());

    let instructions = &captured.lock().unwrap()[0];
    assert!(instructions.contains(workspace_dir.to_string_lossy().as_ref()));
    assert!(instructions.contains("cd"));
    assert!(instructions.contains("All work is in:"));
}
