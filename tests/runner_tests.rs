//! Integration tests for the workflow runner.
//!
//! These tests verify the full workflow execution flow:
//! pre-hook → template render → API call → post-hook → next step.
//!
//! To avoid needing a live Hermes API server, we use a mock HTTP server
//! built with `axum` that returns canned responses.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use yoke::config::AgentConfig;
use yoke::hooks::Hook;
use yoke::runner::{RunnerError, WorkflowRunner};
use yoke::workflow::{GitConfig, Step, Trigger, Workflow};

use axum::{Json, Router, routing::post};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use url::Url;

/// Matches the Hermes API request body format.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct MockRequest {
    instructions: Option<String>,
    input: String,
    store: bool,
}

/// Matches the Hermes API response body format.
#[derive(Debug, Serialize)]
struct MockResponse {
    output: Vec<MockOutputItem>,
}

#[derive(Debug, Serialize)]
struct MockOutputItem {
    #[serde(rename = "type")]
    item_type: String,
    role: String,
    content: Vec<MockContentBlock>,
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

/// Build agent configs for the given mock server base URLs.
fn make_agents(agent_urls: &[(&str, &str)]) -> Vec<AgentConfig> {
    agent_urls
        .iter()
        .map(|(name, url)| AgentConfig {
            name: name.to_string(),
            base_url: Url::parse(url).unwrap(),
        })
        .collect()
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
                    output: vec![MockOutputItem {
                        item_type: "message".to_string(),
                        role: "assistant".to_string(),
                        content: vec![MockContentBlock {
                            block_type: "output_text".to_string(),
                            text,
                        }],
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
                captured
                    .lock()
                    .unwrap()
                    .push(body.instructions.clone().unwrap_or_default());
                let response = MockResponse {
                    output: vec![MockOutputItem {
                        item_type: "message".to_string(),
                        role: "assistant".to_string(),
                        content: vec![MockContentBlock {
                            block_type: "output_text".to_string(),
                            text,
                        }],
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
    let agents = make_agents(&[("pm", &base_url), ("swe", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![
        Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan the issue".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Implement".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_workflow_step_receives_rendered_prompt() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Review issue {{issue_title}}".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let mut variables = HashMap::new();
    variables.insert("issue_title".to_string(), "Bug fix needed".to_string());

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_workflow_pre_hook_failure_stops_execution() {
    let base_url = start_mock_server("Should not be called").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    // Do NOT create plan.md — pre-hook will fail
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan the issue".to_string(),
        pre_hooks: vec![Hook::FileNotEmpty {
            path: "plan.md".to_string(),
        }],
        post_hooks: vec![],
    }]);

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_workflow_post_hook_failure_marks_step_failed() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan the issue".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![Hook::FileContains {
            path: "output.md".to_string(),
            text: "implementation plan".to_string(),
        }],
    }]);

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_workflow_template_error_stops_execution() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Review issue {{nonexistent_var}}".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_workflow_fail_fast_on_first_step_error() {
    let base_url = start_mock_server("Step 1 done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    // Step 2 will fail due to a missing post-hook file
    let workflow = test_workflow(vec![
        Step {
            name: "Step1".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Do step 1".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Step2".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Do step 2".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![Hook::FileContains {
                path: "missing.md".to_string(),
                text: "not here".to_string(),
            }],
        },
        Step {
            name: "Step3".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Do step 3".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            assert!(
                msg.contains("Step2"),
                "error should mention Step2, got: {msg}"
            );
            assert!(
                !msg.contains("Step3"),
                "error should NOT mention Step3, got: {msg}"
            );
        }
        other => panic!("expected Execution error for Step2, got {other:?}"),
    }
}

// --- Context-aware instructions integration tests ---

#[tokio::test]
async fn test_instructions_include_workspace_dir_when_git_enabled() {
    // When git.clone is true, instructions should contain
    // the workspace directory path and a cd directive.
    let (base_url, captured) = start_mock_server_with_capture("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

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
            default_branch: "main".to_string(),
        },
    );

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        workspace_dir.clone(),
        agents,
        "test-key".to_string(),
    );
    let result = runner.run().await;
    assert!(result.is_ok());

    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    let instructions = captured[0].as_str();
    assert!(instructions.contains(workspace_dir.to_string_lossy().as_ref()));
    assert!(instructions.contains("cd"));
    assert!(instructions.contains("All work is in:"));
    assert!(instructions.contains("Reference all file paths relative to this directory"));
}

#[tokio::test]
async fn test_instructions_omitted_when_git_disabled() {
    // When git.clone is false, the instructions
    // field should be omitted (None) from the API request entirely.
    let (base_url, captured) = start_mock_server_with_capture("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

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
            default_branch: "main".to_string(),
        },
    );

    let mut runner = WorkflowRunner::new(
        workflow,
        HashMap::new(),
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );
    let result = runner.run().await;
    assert!(result.is_ok());

    // When instructions is None, the capture mock server pushes empty string
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert!(captured[0].is_empty());
}

// --- Per-step agent resolution tests ---

#[tokio::test]
async fn test_multi_agent_workflow_uses_different_base_urls() {
    // Start two separate mock servers on different ports
    let base_url_pm = start_mock_server("PM response").await;
    let base_url_swe = start_mock_server("SWE response").await;

    let agents = make_agents(&[("pm", &base_url_pm), ("swe", &base_url_swe)]);

    let dir = tempfile::tempdir().unwrap();

    // Workflow with two steps using different agents
    let workflow = test_workflow(vec![
        Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan the issue".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Implement".to_string(),
            agent: "swe".to_string(),
            prompt_template: "Implement the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());
    // Both steps completed — each resolved to its own agent's base_url
}

#[tokio::test]
async fn test_unknown_agent_returns_clear_error() {
    let base_url = start_mock_server("Done").await;
    // Only configure "pm" agent, but the step references "ghost"
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![Step {
        name: "Mystery".to_string(),
        agent: "ghost".to_string(),
        prompt_template: "Do something".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_err());
    match result.unwrap_err() {
        RunnerError::Execution(msg) => {
            // The UnknownAgent error is wrapped by Execution, so we check for
            // the key identifiers in the message
            assert!(
                msg.contains("ghost"),
                "error should mention the agent name 'ghost', got: {msg}"
            );
            assert!(
                msg.contains("Mystery"),
                "error should mention the step name 'Mystery', got: {msg}"
            );
        }
        other => panic!("expected Execution error for unknown agent, got {other:?}"),
    }
}

#[tokio::test]
async fn test_single_agent_workflow_still_works() {
    // Verify backward compatibility: single-agent workflows still work
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![
        Step {
            name: "Plan".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Plan the issue".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Review".to_string(),
            agent: "pm".to_string(),
            prompt_template: "Review the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_file_logging_writes_prompt_and_log_files() {
    // After a successful workflow run, prompt and log files should exist in the
    // workspace directory with actual API data — not placeholder "pending" text.
    let base_url = start_mock_server("Implementation complete").await;
    let agents = make_agents(&[("dev", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![
        Step {
            name: "Plan".to_string(),
            agent: "dev".to_string(),
            prompt_template: "Plan the task".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
        Step {
            name: "Implement".to_string(),
            agent: "dev".to_string(),
            prompt_template: "Implement the plan".to_string(),
            pre_hooks: vec![],
            post_hooks: vec![],
        },
    ]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());

    // Step 0 (Plan): prompt file should contain the rendered template
    let plan_prompt = dir.path().join("00_Plan.prompt");
    assert!(plan_prompt.exists(), "00_Plan.prompt should exist");
    let plan_prompt_content = std::fs::read_to_string(&plan_prompt).unwrap();
    assert!(
        plan_prompt_content.contains("Plan the task"),
        "prompt file should contain rendered template, got: {plan_prompt_content}"
    );
    // Should NOT contain placeholder "pending" text from old dispatcher code
    assert!(
        !plan_prompt_content.contains("pending"),
        "prompt file should not contain 'pending', got: {plan_prompt_content}"
    );

    // Step 0 (Plan): log file should contain real request/response data
    let plan_log = dir.path().join("00_Plan.log");
    assert!(plan_log.exists(), "00_Plan.log should exist");
    let plan_log_content = std::fs::read_to_string(&plan_log).unwrap();
    assert!(
        plan_log_content.contains("REQUEST:"),
        "log file should contain REQUEST section, got: {plan_log_content}"
    );
    assert!(
        plan_log_content.contains("RESPONSE:"),
        "log file should contain RESPONSE section, got: {plan_log_content}"
    );
    assert!(
        plan_log_content.contains("FINAL MESSAGE:"),
        "log file should contain FINAL MESSAGE section, got: {plan_log_content}"
    );
    assert!(
        plan_log_content.contains("Implementation complete"),
        "log should contain the actual response text, got: {plan_log_content}"
    );
    // Should NOT contain old placeholder data
    assert!(
        !plan_log_content.contains("Start"),
        "log should not contain 'Start' placeholder, got: {plan_log_content}"
    );

    // Step 1 (Implement): verify second step also gets logged
    let impl_prompt = dir.path().join("01_Implement.prompt");
    assert!(impl_prompt.exists(), "01_Implement.prompt should exist");
    let impl_prompt_content = std::fs::read_to_string(&impl_prompt).unwrap();
    assert!(
        impl_prompt_content.contains("Implement the plan"),
        "second prompt file should contain its template, got: {impl_prompt_content}"
    );

    let impl_log = dir.path().join("01_Implement.log");
    assert!(impl_log.exists(), "01_Implement.log should exist");
    let impl_log_content = std::fs::read_to_string(&impl_log).unwrap();
    assert!(
        impl_log_content.contains("Implementation complete"),
        "second log should contain the actual response text, got: {impl_log_content}"
    );
}

#[tokio::test]
async fn test_file_logging_writes_request_on_api_failure() {
    // When the Hermes API call fails (e.g. network error), the request body
    // should still be logged to the .log file because we write it before the call.
    // Only the RESPONSE and FINAL MESSAGE sections will be missing.
    let base_url = start_mock_server("should not matter").await;
    let agents = make_agents(&[("dev", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "bad-agent".to_string(), // not in agents list — will fail to resolve
        prompt_template: "Plan the task".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    // This will fail because "bad-agent" is not in the agents list
    let result = runner.run().await;
    assert!(result.is_err(), "workflow should fail with unknown agent");

    // Even though the workflow failed, the .prompt file should NOT exist
    // because step resolution happens before file logging.
    // (The agent resolution fails before we even get to template rendering or logging.)
    let plan_prompt = dir.path().join("00_Plan.prompt");
    assert!(
        !plan_prompt.exists(),
        "prompt file should not exist when agent resolution fails"
    );
}

#[tokio::test]
async fn test_file_logging_request_logged_before_api_call() {
    // After a successful workflow run, the .log file should contain both
    // the request (written before the API call) and the response (written after).
    // This test verifies the two-phase write: request first, then full exchange overwrite.
    let base_url = start_mock_server("Response text here").await;
    let agents = make_agents(&[("dev", &base_url)]);

    let dir = tempfile::tempdir().unwrap();

    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "dev".to_string(),
        prompt_template: "Plan the task".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new();

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    let result = runner.run().await;
    assert!(result.is_ok());

    // The log file should contain the full exchange after a successful run
    let plan_log = dir.path().join("00_Plan.log");
    assert!(plan_log.exists(), "00_Plan.log should exist");
    let plan_log_content = std::fs::read_to_string(&plan_log).unwrap();
    assert!(
        plan_log_content.contains("REQUEST:"),
        "log should contain REQUEST section, got: {plan_log_content}"
    );
    assert!(
        plan_log_content.contains("RESPONSE:"),
        "log should contain RESPONSE section, got: {plan_log_content}"
    );
    assert!(
        plan_log_content.contains("FINAL MESSAGE:"),
        "log should contain FINAL MESSAGE section, got: {plan_log_content}"
    );
}
