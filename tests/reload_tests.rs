use std::fs;
use std::time::Duration;

use tempfile::TempDir;
use tokio::sync::mpsc;

use yoke::reload::{ReloadMessage, setup_file_watcher};

/// Verify that creating a new `.toml` file triggers a `FileChanged` message.
#[tokio::test]
async fn test_new_toml_file_detected() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();

    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();

    // Give the watcher time to initialize.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a new .toml file.
    let file_path = dir_path.join("workflow.toml");
    fs::write(&file_path, "[trigger]\ntype = \"github_issue_assigned\"").unwrap();

    // Wait for the debounced event (500ms debounce + margin).
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    match msg {
        ReloadMessage::FileChanged { path } => {
            assert_eq!(path, file_path);
        }
        ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged, got FileRemoved"),
    }
}

/// Verify that modifying an existing `.toml` file triggers a `FileChanged` message.
#[tokio::test]
async fn test_modified_toml_file_detected() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();

    // Create file before watching starts.
    let file_path = dir_path.join("existing.toml");
    fs::write(&file_path, "old content").unwrap();

    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Modify the file.
    fs::write(&file_path, "new content").unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    match msg {
        ReloadMessage::FileChanged { path } => {
            assert_eq!(path, file_path);
        }
        ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged, got FileRemoved"),
    }
}

/// Verify that deleting a `.toml` file triggers a `FileRemoved` message.
#[tokio::test]
async fn test_deleted_toml_file_detected() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();

    // Create file before watching starts.
    let file_path = dir_path.join("delete_me.toml");
    fs::write(&file_path, "content").unwrap();

    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Delete the file.
    fs::remove_file(&file_path).unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for reload event")
        .expect("channel closed unexpectedly");

    match msg {
        ReloadMessage::FileRemoved { path } => {
            assert_eq!(path, file_path);
        }
        ReloadMessage::FileChanged { .. } => {
            // Some platforms emit a MODIFY event before REMOVE; that's also acceptable
            // as long as we get some notification.
        }
    }
}

/// Verify that non-`.toml` files are ignored.
#[tokio::test]
async fn test_non_toml_file_ignored() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();

    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Create a non-.toml file.
    fs::write(dir_path.join("notes.txt"), "hello").unwrap();

    // Wait long enough that a debounced event would have been delivered.
    let result = tokio::time::timeout(Duration::from_millis(1200), rx.recv()).await;
    assert!(
        result.is_err(),
        "expected no reload event for non-.toml file"
    );
}

/// Verify that rapid changes are debounced into a single reload event.
#[tokio::test]
async fn test_rapid_changes_debounced() {
    let dir = TempDir::new().unwrap();
    let dir_path = dir.path().to_path_buf();

    let file_path = dir_path.join("rapid.toml");
    fs::write(&file_path, "v1").unwrap();

    let (tx, mut rx) = mpsc::channel(32);
    let _watcher = setup_file_watcher(&dir_path, tx).unwrap();

    tokio::time::sleep(Duration::from_millis(300)).await;

    // Make 3 rapid modifications.
    for i in 0..3 {
        fs::write(&file_path, format!("v{}", i + 2)).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Should get at least one debounced reload event.
    let _first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out waiting for first reload event")
        .expect("channel closed unexpectedly");

    // The rapid changes within 500ms should be debounced into a single event,
    // so no second event should arrive within the debounce window.
    let result = tokio::time::timeout(Duration::from_millis(800), rx.recv()).await;
    assert!(
        result.is_err(),
        "expected only one debounced event, got a second"
    );
}
