//! Integration tests for the dispatcher main loop.
//!
//! These tests verify the full dispatch flow:
//! - Deduplication: duplicate events are rejected
//! - Concurrency limiting: semaphore caps parallel workflows
//! - Persistence: completed/failed events are written to disk
//! - Graceful shutdown: dispatcher drains in-flight tasks before exiting
//! - Full lifecycle: event flows through dedup → permit → spawn → complete

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use yoke::dispatcher::{
    DispatchMessage, Dispatcher, build_dedup_key, extract_event_id, load_persistence,
    new_dedup_sets,
};
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
        variables: std::collections::HashMap::new(),
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

// --- Test: Full dispatch flow (send, dedup, spawn, complete, persist) ---

#[tokio::test]
async fn test_full_dispatch_flow_completes_and_persists() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn the dispatcher loop
    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send a single event
    let msg = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-42",
    );
    tx.send(msg).await.unwrap();

    // Give the dispatcher time to process
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Close the channel to stop the dispatcher
    drop(tx);
    handle.await.unwrap();

    // Verify the event was tracked in completed set
    let sets = dedup_sets.read().await;
    let event = make_event(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-42",
    );
    let key = build_dedup_key("owner", "repo", &extract_event_id(&event));
    assert!(
        sets.completed.contains(&key),
        "event should be in completed set"
    );
    assert!(
        sets.in_flight.is_empty(),
        "in_flight should be empty after completion"
    );
}

// --- Test: Duplicate rejection ---

#[tokio::test]
async fn test_duplicate_event_rejected() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send the same event twice
    let event_type = TriggerType::GithubIssueAssigned {
            assigned_to: None,
        };
    let msg1 = make_message(event_type.clone(), "issue-42");
    let msg2 = make_message(event_type, "issue-42");

    tx.send(msg1).await.unwrap();
    tx.send(msg2).await.unwrap();

    // Give the dispatcher time to process both messages
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Close the channel
    drop(tx);
    handle.await.unwrap();

    // Verify only one event is in completed (second was rejected as duplicate)
    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        1,
        "only one event should be in completed set"
    );
}

// --- Test: Concurrency limit caps parallel workflows ---

#[tokio::test]
async fn test_concurrency_limit() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // Limit to 1 concurrent workflow
    let dispatcher = test_dispatcher(dedup_sets.clone(), 1, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send two events with different event IDs
    let msg1 = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-42",
    );
    let msg2 = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-43",
    );

    tx.send(msg1).await.unwrap();
    tx.send(msg2).await.unwrap();

    // Give time for both to process
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Close the channel
    drop(tx);
    handle.await.unwrap();

    // Both events should eventually complete
    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        2,
        "both events should be in completed set"
    );
}

// --- Test: Persistence of completed events ---

#[tokio::test]
async fn test_completed_events_persisted_to_disk() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    let msg = make_message(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-42",
    );
    tx.send(msg).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    drop(tx);
    handle.await.unwrap();

    // Verify completed.json was written to disk
    let completed_path = workdir.path().join("completed.json");
    assert!(
        completed_path.exists(),
        "completed.json should exist on disk"
    );
    let loaded: HashSet<String> =
        serde_json::from_str(&std::fs::read_to_string(&completed_path).unwrap()).unwrap();
    let event = make_event(
        TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
        "issue-42",
    );
    let key = build_dedup_key("owner", "repo", &extract_event_id(&event));
    assert!(
        loaded.contains(&key),
        "completed.json should contain the event key"
    );
}

// --- Test: Graceful shutdown drains in-flight tasks ---

#[tokio::test]
async fn test_graceful_shutdown_drains_in_flight() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send an event
    let msg = make_message(
        TriggerType::GithubPullRequestReview,
        "pr-7-review-999",
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

// --- Test: Channel closed stops dispatcher ---

#[tokio::test]
async fn test_dispatcher_stops_when_channel_closed() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send an event and close the channel
    let msg = make_message(
        TriggerType::GithubIssueCommentMention {
                mentioned_user: None,
            },
        "issue-42-comment-12345",
    );
    tx.send(msg).await.unwrap();
    drop(tx); // Close channel

    // Dispatcher should stop cleanly
    handle.await.unwrap();

    // Event should have been processed
    let sets = dedup_sets.read().await;
    assert!(
        !sets.completed.is_empty(),
        "event should be in completed set after channel close"
    );
}

// --- Test: Multiple different events are all processed ---

#[tokio::test]
async fn test_multiple_different_events_processed() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send multiple different events
    let events = vec![
        make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            "issue-42",
        ),
        make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            "issue-43",
        ),
        make_message(
            TriggerType::GithubPullRequestReview,
            "pr-7-review-999",
        ),
    ];

    for msg in events {
        tx.send(msg).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    drop(tx);
    handle.await.unwrap();

    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        3,
        "all three events should be in completed set"
    );
}

// --- Test: on_workflow_complete transitions state correctly ---

#[tokio::test]
async fn test_on_workflow_complete_success() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    // Manually mark an event as in-flight
    {
        let mut sets = dedup_sets.write().await;
        sets.mark_in_flight("owner/repo/42");
    }

    // Complete it
    dispatcher
        .on_workflow_complete("owner/repo/42", Ok(()))
        .await;

    let sets = dedup_sets.read().await;
    assert!(
        sets.completed.contains("owner/repo/42"),
        "key should be in completed set"
    );
    assert!(
        !sets.in_flight.contains("owner/repo/42"),
        "key should no longer be in in_flight set"
    );

    // Verify persistence
    let loaded = load_persistence(workdir.path());
    assert!(
        loaded.completed.contains("owner/repo/42"),
        "key should be persisted in completed set"
    );
}

#[tokio::test]
async fn test_on_workflow_complete_failure() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    // Manually mark an event as in-flight
    {
        let mut sets = dedup_sets.write().await;
        sets.mark_in_flight("owner/repo/42");
    }

    // Fail it
    dispatcher
        .on_workflow_complete("owner/repo/42", Err("something went wrong".to_string()))
        .await;

    let sets = dedup_sets.read().await;
    assert!(
        sets.permanently_failed.contains("owner/repo/42"),
        "key should be in permanently_failed set"
    );
    assert!(
        !sets.in_flight.contains("owner/repo/42"),
        "key should no longer be in in_flight set"
    );

    // Verify persistence
    let loaded = load_persistence(workdir.path());
    assert!(
        loaded.permanently_failed.contains("owner/repo/42"),
        "key should be persisted in permanently_failed set"
    );
}

// --- Test: Active count with concurrent events ---

#[tokio::test]
async fn test_dispatcher_active_count_with_concurrent_events() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // Use concurrency limit of 2 so the semaphore is in play
    let dispatcher = test_dispatcher(dedup_sets.clone(), 2, PathBuf::from(workdir.path()));

    // Use run_with_permit directly since spawn_workflow is fire-and-forget
    // and active_count is decremented in run_with_permit
    assert_eq!(
        dispatcher.active_count(),
        0,
        "initial active count should be 0"
    );

    let result = dispatcher.run_with_permit(async { 42 }).await;
    assert_eq!(result, 42);
    assert_eq!(
        dispatcher.active_count(),
        0,
        "active count should return to 0 after run_with_permit completes"
    );
}

// --- Test: GitLab events are also processed ---

#[tokio::test]
async fn test_gitlab_event_dispatched() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    let msg = make_message(
        TriggerType::GitlabIssueAssigned { assigned_to: None },
        "issue-7",
    );
    tx.send(msg).await.unwrap();

    tokio::time::sleep(Duration::from_millis(200)).await;

    drop(tx);
    handle.await.unwrap();

    let sets = dedup_sets.read().await;
    let event = make_event(
        TriggerType::GitlabIssueAssigned { assigned_to: None },
        "issue-7",
    );
    let key = build_dedup_key("owner", "repo", &extract_event_id(&event));
    assert!(
        sets.completed.contains(&key),
        "GitLab event should be in completed set"
    );
}

// --- Test: Unlimited concurrency high-throughput stress test ---

#[tokio::test]
async fn test_unlimited_throughput() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(2000);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send 500 unique events through the dispatcher with max_concurrent=0 (unlimited)
    let total_events = 500;
    for i in 0..total_events {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            &format!("issue-{i}"),
        );
        tx.send(msg).await.unwrap();
    }

    // Give the dispatcher time to process all events
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Close the channel
    drop(tx);
    handle.await.unwrap();

    // All events should be in the completed set, none in in_flight
    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        total_events,
        "all {total_events} events should be in completed set"
    );
    assert!(
        sets.in_flight.is_empty(),
        "in_flight should be empty after completion"
    );

    // Verify persistence to disk
    let loaded = load_persistence(workdir.path());
    assert_eq!(
        loaded.completed.len(),
        total_events,
        "all events should be persisted to completed.json"
    );
}

// --- Test: High-concurrency semaphore stress test ---

#[tokio::test]
async fn test_concurrency_stress_with_semaphore() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // Limit to 4 concurrent workflows
    let dispatcher = test_dispatcher(dedup_sets.clone(), 4, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(200);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send 50 unique events — they should all eventually complete with concurrency limit of 4
    let total_events = 50;
    for i in 0..total_events {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            &format!("issue-{i}"),
        );
        tx.send(msg).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    drop(tx);
    handle.await.unwrap();

    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        total_events,
        "all events should complete even with concurrency limit"
    );
    assert!(
        sets.in_flight.is_empty(),
        "in_flight should be empty after completion"
    );
}

// --- Test: Dispatcher failure path via on_workflow_complete ---

#[tokio::test]
async fn test_failure_state_transition_via_on_workflow_complete() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    // Mark event as in-flight, then fail it
    {
        let mut sets = dedup_sets.write().await;
        sets.mark_in_flight("owner/repo/100");
        sets.mark_in_flight("owner/repo/101");
    }

    // One succeeds, one fails
    dispatcher
        .on_workflow_complete("owner/repo/100", Ok(()))
        .await;
    dispatcher
        .on_workflow_complete("owner/repo/101", Err("simulation failure".to_string()))
        .await;

    let sets = dedup_sets.read().await;
    assert!(
        sets.completed.contains("owner/repo/100"),
        "successful event should be in completed"
    );
    assert!(
        sets.permanently_failed.contains("owner/repo/101"),
        "failed event should be in permanently_failed"
    );
    assert!(
        sets.in_flight.is_empty(),
        "in_flight should be empty after transitions"
    );

    // Verify both persisted states on disk
    let loaded = load_persistence(workdir.path());
    assert!(
        loaded.completed.contains("owner/repo/100"),
        "completed event should be persisted"
    );
    assert!(
        loaded.permanently_failed.contains("owner/repo/101"),
        "failed event should be persisted"
    );
}

// --- Test: Permits released on task completion (concurrency semaphore) ---

#[tokio::test]
async fn test_permits_released_after_completion() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // Use concurrency limit of 2 so the semaphore is in play
    let dispatcher = test_dispatcher(dedup_sets.clone(), 2, PathBuf::from(workdir.path()));

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let handle = tokio::spawn(async move {
        dispatcher
            .run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send 6 events with concurrency limit of 2
    // They should all complete because permits are released after each task
    for i in 0..6 {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            &format!("issue-{i}"),
        );
        tx.send(msg).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(500)).await;

    drop(tx);
    handle.await.unwrap();

    let sets = dedup_sets.read().await;
    assert_eq!(
        sets.completed.len(),
        6,
        "all 6 events should complete as permits are released"
    );
    assert!(
        sets.in_flight.is_empty(),
        "no events should be in_flight after completion"
    );
}

// --- Test: active_count returns to 0 after spawn_workflow tasks complete ---

#[tokio::test]
async fn test_active_count_decrements_after_spawn_workflow() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    let dispatcher = test_dispatcher(dedup_sets.clone(), 2, PathBuf::from(workdir.path()));

    assert_eq!(
        dispatcher.active_count(),
        0,
        "active_count should start at 0"
    );

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let disp = dispatcher.clone();
    let handle = tokio::spawn(async move {
        disp.run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send 3 events — each will increment active_count on spawn
    for i in 0..3 {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            &format!("issue-{}0", 100 + i),
        );
        tx.send(msg).await.unwrap();
    }

    // Give the dispatcher time to process all events and for active_count to return to 0
    tokio::time::sleep(Duration::from_millis(500)).await;

    drop(tx);

    // active_count should have returned to 0 after all spawned tasks completed
    assert_eq!(
        dispatcher.active_count(),
        0,
        "active_count should return to 0 after all spawned workflows complete"
    );

    shutdown_tx.send(true).unwrap();
    let _ = handle.await;
}

// --- Test: active_count returns to 0 after spawn with unlimited concurrency ---

#[tokio::test]
async fn test_active_count_stays_zero_with_unlimited_concurrency() {
    let workdir = make_workdir();
    let dedup_sets = new_dedup_sets();
    // max_concurrent = 0 means no semaphore, so acquire_permit returns None
    // and active_count should never be incremented
    let dispatcher = test_dispatcher(dedup_sets.clone(), 0, PathBuf::from(workdir.path()));

    assert_eq!(
        dispatcher.active_count(),
        0,
        "active_count should be 0 when unlimited concurrency"
    );

    let (tx, rx) = tokio::sync::mpsc::channel(100);
    let (_shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    let disp = dispatcher.clone();
    let handle = tokio::spawn(async move {
        disp.run_with_drain(rx, &mut shutdown_rx, Duration::from_secs(30))
            .await;
    });

    // Send multiple events — with unlimited concurrency, active_count stays 0
    for i in 0..5 {
        let msg = make_message(
            TriggerType::GithubIssueAssigned {
            assigned_to: None,
        },
            &format!("issue-{i}00"),
        );
        tx.send(msg).await.unwrap();
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    drop(tx);
    handle.await.unwrap();

    // With unlimited concurrency (no semaphore), active_count never increments
    assert_eq!(
        dispatcher.active_count(),
        0,
        "active_count should remain 0 with unlimited concurrency"
    );
}
