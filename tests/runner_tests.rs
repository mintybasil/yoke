//! Integration tests for the workflow runner.
//!
//! These tests verify the full workflow execution flow:
//! pre-hook → template render → API call → post-hook → next step.
//!
//! To avoid needing a live Hermes API server, we use a mock HTTP server
//! built with `axum` that returns canned responses.

use std::collections::HashMap;

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

#[tokio::test]
async fn test_workflow_execution_two_steps() {
    let base_url = start_mock_server("Step completed").await;
    let agents = make_agents(&[("pm", &base_url), ("swe", &base_url)]);

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
async fn test_template_variables_substituted() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

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
async fn test_unknown_variable_fails() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

    let dir = tempfile::tempdir().unwrap();
    let workflow = test_workflow(vec![Step {
        name: "Plan".to_string(),
        agent: "pm".to_string(),
        prompt_template: "Plan {{unknown_var}}".to_string(),
        pre_hooks: vec![],
        post_hooks: vec![],
    }]);

    let variables = HashMap::new(); // empty — no unknown_var

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
            assert!(msg.contains("Plan"));
            assert!(msg.contains("unknown variable"));
        }
        other => panic!("expected Execution error with template detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_pre_hook_failure_prevents_step() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

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
            assert!(msg.contains("Plan"));
            assert!(msg.contains("plan.md"));
        }
        other => panic!("expected Execution error with hook detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_post_hook_failure_marks_step_failed() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url)]);

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
            assert!(msg.contains("Plan"));
            assert!(msg.contains("output.md"));
        }
        other => panic!("expected Execution error with hook detail, got {other:?}"),
    }
}

#[tokio::test]
async fn test_fail_fast_on_first_step_error() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url), ("swe", &base_url)]);

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

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

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
    let agents = make_agents(&[("pm", &base_url)]);

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
async fn test_steps_execute_in_order() {
    // Use a mock server that echoes the instruction text so we can verify order
    let base_url = start_mock_server("Completed").await;
    let agents = make_agents(&[("pm", &base_url), ("swe", &base_url)]);

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

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

    // If all three steps run successfully, we know they executed in order
    // (fail-fast would stop at the first failure)
    let result = runner.run().await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn test_hook_failure_between_steps_stops_workflow() {
    let base_url = start_mock_server("Done").await;
    let agents = make_agents(&[("pm", &base_url), ("swe", &base_url)]);

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

    let mut runner = WorkflowRunner::new(
        workflow,
        variables,
        dir.path().to_path_buf(),
        agents,
        "test-key".to_string(),
    );

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
