use std::fs;
use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;

use yoke::config::Config;
use yoke::reload::{ReloadMessage, WorkflowState, reload_workflows, setup_file_watcher};

fn make_config() -> Config {
    Config::from_str(
        r#"
platform = "github"

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"

[server]
webhook_host = "yoke.example.com"
"#,
    )
    .unwrap()
}

fn github_workflow_toml(name: &str, agent: &str) -> String {
    format!(
        r#"
[trigger]
type = "github_issue_assigned"
assigned_to = "alice"
allowed_users = ["alice"]

[[steps]]
name = "{name}"
agent = "{agent}"
prompt_template = "Do the work"
"#
    )
}

/// Verify that creating a new valid workflow file results in the state being
/// updated after the file watcher detects the change and reload runs.
#[tokio::test]
async fn test_hot_reload_adds_new_workflow() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();
    let config = make_config();

    // Start with one workflow
    let initial_toml = github_workflow_toml("Plan", "pm");
    fs::write(dir_path.join("plan.toml"), &initial_toml).unwrap();

    let initial = reload_workflows(&dir_path, &config).unwrap();
    assert_eq!(initial.len(), 1);

    let state = Arc::new(WorkflowState::new(initial));

    // Set up file watcher
    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a new workflow file
    let new_toml = github_workflow_toml("Implement", "swe");
    fs::write(dir_path.join("implement.toml"), &new_toml).unwrap();

    // Wait for the debounced reload event
    let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    match msg {
        ReloadMessage::FileChanged { path } => {
            assert!(
                path.ends_with("implement.toml"),
                "expected implement.toml, got: {:?}",
                path
            );
        }
        ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged"),
    }

    // Manually reload (simulating what the reload handler in main.rs does)
    match reload_workflows(&dir_path, &config) {
        Ok(new_workflows) => {
            state.update(new_workflows);
        }
        Err(e) => panic!("expected reload to succeed, got: {e}"),
    }

    // Verify the state now contains both workflows
    let loaded = state.load();
    assert_eq!(
        loaded.len(),
        2,
        "expected 2 workflows, got {}",
        loaded.len()
    );
}

/// Verify that adding an invalid workflow file does NOT change the state;
/// the reload attempt fails and the old workflows are preserved.
#[tokio::test]
async fn test_hot_reload_invalid_file_preserves_state() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();
    let config = make_config();

    // Start with one valid workflow
    let initial_toml = github_workflow_toml("Plan", "pm");
    fs::write(dir_path.join("plan.toml"), &initial_toml).unwrap();

    let initial = reload_workflows(&dir_path, &config).unwrap();
    assert_eq!(initial.len(), 1);

    let state = Arc::new(WorkflowState::new(initial));

    // Set up file watcher
    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create an invalid workflow file (unknown agent)
    let bad_toml = r#"
[trigger]
type = "github_issue_assigned"
allowed_users = ["alice"]

[[steps]]
name = "Bad"
agent = "nonexistent_agent"
prompt_template = "This will fail"
"#;
    fs::write(dir_path.join("bad.toml"), bad_toml).unwrap();

    // Wait for the debounced reload event
    let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    match msg {
        ReloadMessage::FileChanged { path } => {
            assert!(path.ends_with("bad.toml"));
        }
        ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged"),
    }

    // Attempt reload — should fail due to unknown agent
    let result = reload_workflows(&dir_path, &config);
    assert!(
        result.is_err(),
        "expected reload to fail for invalid workflow"
    );

    // State remains unchanged (old workflows preserved)
    let loaded = state.load();
    assert_eq!(loaded.len(), 1, "state should still have 1 workflow");
    assert_eq!(
        loaded[0].0,
        dir_path.join("plan.toml").display().to_string()
    );
}

/// Verify that deleting a workflow file triggers a reload that excludes
/// the deleted file from the workflow set.
#[tokio::test]
async fn test_hot_reload_file_removed() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();
    let config = make_config();

    // Start with two workflows
    let toml1 = github_workflow_toml("Plan", "pm");
    let toml2 = github_workflow_toml("Implement", "swe");
    fs::write(dir_path.join("plan.toml"), &toml1).unwrap();
    fs::write(dir_path.join("implement.toml"), &toml2).unwrap();

    let initial = reload_workflows(&dir_path, &config).unwrap();
    assert_eq!(initial.len(), 2);

    let state = Arc::new(WorkflowState::new(initial));

    // Set up file watcher
    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Delete one workflow file
    fs::remove_file(dir_path.join("implement.toml")).unwrap();

    // Wait for the debounced reload event
    let msg = tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    // May get FileChanged (some OS emit MODIFY before REMOVE) or FileRemoved
    let _ = msg; // We just need to know a notification arrived

    // Reload successfully
    match reload_workflows(&dir_path, &config) {
        Ok(new_workflows) => {
            state.update(new_workflows);
        }
        Err(e) => panic!("expected reload to succeed, got: {e}"),
    }

    // Verify the state now has only one workflow
    let loaded = state.load();
    assert_eq!(loaded.len(), 1, "expected 1 workflow after deletion");
}
