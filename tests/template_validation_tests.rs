//! Integration tests for template variable validation at workflow load time.
//!
//! These tests verify that `load_workflows()` rejects workflow TOML files
//! containing unknown template variables, malformed placeholders, or
//! cross-platform variable mismatches — causing a hard exit at startup.

use std::fs;

use yoke::workflow::{WorkflowError, load_workflows};

/// Helper: create a temp dir with a single workflow TOML file, then load it.
fn load_single_workflow(
    toml: &str,
) -> Result<Vec<(String, yoke::workflow::Workflow)>, WorkflowError> {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("test.toml"), toml).unwrap();
    load_workflows(dir.path())
}

#[test]
fn test_valid_workflow_with_known_variables_loads() {
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan {{owner}}/{{repo}}#{{issue_number}}"
"#;
    let result = load_single_workflow(toml);
    assert!(
        result.is_ok(),
        "expected workflow to load, got error: {:?}",
        result.err()
    );
    let workflows = result.unwrap();
    assert_eq!(workflows.len(), 1);
}

#[test]
fn test_workflow_with_unknown_variable_fails_to_load() {
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan {{typo_variable}}"
"#;
    let result = load_single_workflow(toml);
    assert!(result.is_err(), "expected workflow load to fail");
    match result.unwrap_err() {
        WorkflowError::Validation { path, message } => {
            assert!(
                path.contains("test.toml"),
                "error path should contain file name: {path}"
            );
            assert!(
                message.contains("unknown template variable"),
                "error should mention unknown variable, got: {message}"
            );
            assert!(
                message.contains("typo_variable"),
                "error should mention the variable name, got: {message}"
            );
            assert!(
                message.contains("Plan"),
                "error should mention the step name, got: {message}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn test_workflow_with_syntax_error_fails_to_load() {
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan {{owner}}/{{unclosed"
"#;
    let result = load_single_workflow(toml);
    assert!(result.is_err(), "expected workflow load to fail");
    match result.unwrap_err() {
        WorkflowError::Validation { message, .. } => {
            assert!(
                message.contains("syntax error"),
                "error should mention syntax error, got: {message}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn test_workflow_with_empty_placeholder_fails_to_load() {
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan {{}}"
"#;
    let result = load_single_workflow(toml);
    assert!(result.is_err(), "expected workflow load to fail");
    match result.unwrap_err() {
        WorkflowError::Validation { message, .. } => {
            assert!(
                message.contains("syntax error"),
                "error should mention syntax error, got: {message}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn test_cross_platform_variable_fails_to_load() {
    // Using GitHub-specific variable pr_number in a GitLab workflow
    let toml = r#"
[trigger]
type = "gitlab_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Review PR {{pr_number}}"
"#;
    let result = load_single_workflow(toml);
    assert!(result.is_err(), "expected workflow load to fail");
    match result.unwrap_err() {
        WorkflowError::Validation { message, .. } => {
            assert!(
                message.contains("unknown template variable"),
                "error should mention unknown variable, got: {message}"
            );
            assert!(
                message.contains("pr_number"),
                "error should mention the variable name, got: {message}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn test_gitlab_workflow_with_valid_variables_loads() {
    let toml = r#"
[trigger]
type = "gitlab_issue_assigned"
assigned_to = "alice"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan issue {{issue_iid}} for {{owner}}/{{repo}}"
"#;
    let result = load_single_workflow(toml);
    assert!(
        result.is_ok(),
        "expected workflow to load, got error: {:?}",
        result.err()
    );
}

#[test]
fn test_multiple_steps_one_bad_variable_fails() {
    // Step 2 has a typo, the whole workflow should fail
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan {{owner}}/{{repo}}#{{issue_number}}"

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = "Implement for {{issue_nubmer}}"
"#;
    let result = load_single_workflow(toml);
    assert!(result.is_err(), "expected workflow load to fail");
    match result.unwrap_err() {
        WorkflowError::Validation { message, .. } => {
            assert!(
                message.contains("unknown template variable"),
                "error should mention unknown variable, got: {message}"
            );
            assert!(
                message.contains("issue_nubmer"),
                "error should mention the typo variable, got: {message}"
            );
            assert!(
                message.contains("Implement"),
                "error should mention the step with the typo, got: {message}"
            );
        }
        other => panic!("expected Validation error, got: {other}"),
    }
}

#[test]
fn test_workflow_with_no_variables_loads() {
    // Templates without any {{}} placeholders should load fine
    let toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Just a plain text prompt"
"#;
    let result = load_single_workflow(toml);
    assert!(
        result.is_ok(),
        "expected workflow to load, got error: {:?}",
        result.err()
    );
}
