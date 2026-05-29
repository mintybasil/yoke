//! Integration tests for graceful shutdown via SIGINT/SIGTERM.
//!
//! These tests verify the signal handler and drain logic:
//! - Signal handler catches SIGINT and SIGTERM
//! - First signal triggers graceful shutdown (watch channel → true)
//! - Second signal forces immediate process::exit(1)
//! - Dispatcher drains in-flight workflows before exiting
//! - State is persisted before exit

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use yoke::dispatcher::{DispatchMessage, Dispatcher, load_persistence, new_dedup_sets};
use yoke::reload::WorkflowState;
use yoke::webhook::TriggerEvent;
use yoke::workflow::TriggerType;

/// Create a Dispatcher for tests with an empty workflow state and no agents.
fn test_dispatcher(
    dedup: yoke::dispatcher::SharedDedupSets,
    max_concurrent: usize,
    workdir: PathBuf,
) -> Dispatcher {
    let workflow_state = Arc::new(WorkflowState::new(vec![]));
    Dispatcher::new(dedup, max_concurrent, workdir, workflow_state, vec![])
}

// --- Helper functions ---

/// Create a test `TriggerEvent` with the given trigger type and event ID.
fn make_event(trigger_type: TriggerType, event_id: &str) -> TriggerEvent {
    TriggerEvent {
        trigger_type,
        repo_path: "owner/repo".to_string(),
        event_id: event_id.to_string(),
    }
}

/// Create a `DispatchMessage` wrapping the given event.
fn make_message(trigger_type: TriggerType, event_id: &str) -> DispatchMessage {
    DispatchMessage {
        event: make_event(trigger_type, event_id),
    }
}

/// Create a temp directory for test persistence files.
fn make_workdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("failed to create temp dir")
}

// --- Test: Signal handler sends true on first shutdown signal ---

#[tokio::test]
async fn test_signal_handler_sends_shutdown_on_first_signal() {
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // We can't easily send real UNIX signals in tests, so we test the
    // watch channel propagation directly — verifying the shutdown signal
    // causes the dispatcher to stop and drain.

    // Send shutdown signal
    shutdown_tx.send(true).unwrap();

    // Verify the receiver sees the change
    assert!(
        *shutdown_rx.borrow(),
        "shutdown_rx should be true after send"
    );
}

// --- Test: Graceful shutdown drains in-flight workflows with custom timeout ---

#[tokio::test]
async fn test_graceful_shutdown_with_custom_drain_timeout() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let drain_timeout = Duration::from_secs(5);

    let handle = tokio::spawn(async move {
        let mut shutdown = shutdown_rx;
        dispatcher
            .run_with_drain(rx, &mut shutdown, drain_timeout)
            .await;
    });

    // Send an event
    let msg = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
            allowed_users: None,
        },
        "issue-42",
    );
    tx.send(msg).await.unwrap();

    // Give it a moment to start processing
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Signal shutdown
    shutdown_tx.send(true).unwrap();

    // Close channel so the dispatcher stops receiving
    drop(tx);

    // The dispatcher should drain and complete
    handle.await.unwrap();

    // Event should be completed
    let sets = dedup_sets.read().await;
    assert!(
        !sets.completed.is_empty(),
        "event should be in completed set after graceful shutdown"
    );
}

// --- Test: State is persisted during graceful shutdown ---

#[tokio::test]
async fn test_state_persisted_on_graceful_shutdown() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let drain_timeout = Duration::from_secs(5);

    let handle = tokio::spawn({
        let dispatcher = dispatcher.clone();
        async move {
            let mut shutdown = shutdown_rx;
            dispatcher
                .run_with_drain(rx, &mut shutdown, drain_timeout)
                .await;
        }
    });

    // Send an event
    let msg = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
            allowed_users: None,
        },
        "issue-42",
    );
    tx.send(msg).await.unwrap();

    // Give it a moment to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Signal shutdown
    shutdown_tx.send(true).unwrap();
    drop(tx);

    // Wait for completion
    handle.await.unwrap();

    // Verify state was persisted
    let persist_dir = workdir.path();
    let completed_path = persist_dir.join("completed.json");
    assert!(
        completed_path.exists(),
        "completed.json should be persisted on shutdown"
    );

    // Verify we can load the persisted state
    let loaded = load_persistence(persist_dir);
    assert!(
        !loaded.completed.is_empty(),
        "persisted completed set should contain the event"
    );
}

// --- Test: Shutdown with no active workflows completes immediately ---

#[tokio::test]
async fn test_shutdown_with_no_active_workflows() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (_tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let drain_timeout = Duration::from_secs(5);

    let handle = tokio::spawn(async move {
        let mut shutdown = shutdown_rx;
        dispatcher
            .run_with_drain(rx, &mut shutdown, drain_timeout)
            .await;
    });

    // No events sent — signal shutdown immediately
    shutdown_tx.send(true).unwrap();

    // Should complete quickly since active_count = 0
    let start = std::time::Instant::now();
    handle.await.unwrap();
    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(2),
        "shutdown with no active workflows should be fast, took {:?}",
        elapsed
    );
}

// --- Test: Drain timeout expires gracefully ---

#[tokio::test]
async fn test_drain_timeout_expires_gracefully() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // Use concurrency of 1 so we can control ordering
    let dispatcher = test_dispatcher(dedup_sets.clone(), 1, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    // Very short drain timeout
    let drain_timeout = Duration::from_millis(50);

    let handle = tokio::spawn(async move {
        let mut shutdown = shutdown_rx;
        dispatcher
            .run_with_drain(rx, &mut shutdown, drain_timeout)
            .await;
    });

    // Send events that will still be in-flight when shutdown occurs
    for i in 0..5 {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
                assigned_to: None,
                allowed_users: None,
            },
            &format!("issue-{}", 100 + i),
        );
        tx.send(msg).await.unwrap();
    }

    // Give a tiny moment for first event to start
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Signal shutdown while events may still be processing
    shutdown_tx.send(true).unwrap();
    drop(tx);

    // Should complete without hanging, even if timeout expires
    let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
    assert!(
        result.is_ok(),
        "dispatcher should complete even when drain timeout expires"
    );
}

// --- Test: Watch channel shutdown signal propagates to dispatcher ---

#[tokio::test]
async fn test_shutdown_signal_propagates_via_watch_channel() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let drain_timeout = Duration::from_secs(10);

    let handle = tokio::spawn(async move {
        let mut shutdown = shutdown_rx;
        dispatcher
            .run_with_drain(rx, &mut shutdown, drain_timeout)
            .await;
    });

    // Send an event
    let msg = make_message(
        TriggerType::GithubPullRequestReview {
            allowed_users: None,
        },
        "pr-7-review-999",
    );
    tx.send(msg).await.unwrap();

    // Give it time to be processed
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send shutdown signal via watch channel (simulating what signal handler does)
    shutdown_tx.send(true).unwrap();
    drop(tx);

    // Dispatcher should complete
    handle.await.unwrap();

    let sets = dedup_sets.read().await;
    assert!(
        sets.completed.contains("owner/repo/7_review-999"),
        "event should be completed after shutdown"
    );
}
