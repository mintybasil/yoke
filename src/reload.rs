use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

/// Messages sent by the file watcher when workflow files change.
#[derive(Debug, Clone)]
pub enum ReloadMessage {
    /// A workflow `.toml` file was created or modified.
    FileChanged { path: PathBuf },
    /// A workflow `.toml` file was deleted.
    FileRemoved { path: PathBuf },
}

/// Duration to wait after the last file-system event before emitting a reload.
const DEBOUNCE_INTERVAL: Duration = Duration::from_millis(500);

/// Handle to the running file watcher. Drop to stop watching.
///
/// The watcher and bridge thread are stopped when this handle is dropped.
pub struct FileWatcher {
    /// The `notify` watcher — must stay alive to receive events.
    _watcher: notify::RecommendedWatcher,
    /// Handle to the bridge thread that forwards events to the async loop.
    _bridge_join: Option<std::thread::JoinHandle<()>>,
    /// Handle to the debouncing tokio task — aborted on drop.
    _debounce_task: JoinHandle<()>,
}

/// Set up a file watcher on the `workflows_dir` directory.
///
/// The watcher monitors for `.toml` file changes (create, modify, delete) and sends
/// debounced [`ReloadMessage`] events over `tx`. Rapid successive changes within
/// 500ms are collapsed into a single reload event.
///
/// Returns a [`FileWatcher`] handle. The watcher runs in the background; dropping
/// the handle stops the watcher and the debouncing task.
///
/// # Errors
///
/// Returns [`notify::Error`] if the watcher cannot be created or the directory
/// cannot be watched.
pub fn setup_file_watcher(
    workflows_dir: &Path,
    tx: Sender<ReloadMessage>,
) -> notify::Result<FileWatcher> {
    // Bridge channel: notify's callback is synchronous.
    let (raw_tx, raw_rx) = std_mpsc::channel::<Event>();

    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(event) = res
            && is_toml_event(&event)
        {
            // Ignore send errors — the receiver may have been dropped during shutdown.
            let _ = raw_tx.send(event);
        }
    })?;

    watcher.watch(workflows_dir, RecursiveMode::NonRecursive)?;

    // Async channel for the bridge thread -> debounce loop.
    let (async_tx, async_rx) = tokio::sync::mpsc::channel::<RawChangeEvent>(32);

    // Spawn a bridge thread: reads from the sync channel, sends to the async channel.
    // When the watcher is dropped, the sync sender is closed, the bridge thread exits,
    // and it drops the async sender, causing the debounce loop to exit.
    let bridge_join = std::thread::spawn(move || {
        while let Ok(event) = raw_rx.recv() {
            let msg = classify_event(&event);
            if let Some(change) = msg {
                // Use blocking_send since this is a std thread.
                if async_tx.blocking_send(change).is_err() {
                    break; // Receiver dropped.
                }
            }
        }
    });

    // Spawn the debouncing loop on the tokio runtime.
    let debounce_task = tokio::spawn(debounce_loop(async_rx, tx));

    Ok(FileWatcher {
        _watcher: watcher,
        _bridge_join: Some(bridge_join),
        _debounce_task: debounce_task,
    })
}

/// Raw change event from file system, before debouncing.
#[derive(Debug)]
struct RawChangeEvent {
    is_remove: bool,
    path: PathBuf,
}

/// Classify a `notify` event into a `RawChangeEvent`, if applicable.
fn classify_event(event: &Event) -> Option<RawChangeEvent> {
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => {
            let path = event.paths.first()?.clone();
            Some(RawChangeEvent {
                is_remove: false,
                path,
            })
        }
        EventKind::Remove(_) => {
            let path = event.paths.first()?.clone();
            Some(RawChangeEvent {
                is_remove: true,
                path,
            })
        }
        _ => None,
    }
}

/// Run the debouncing loop that coalesces rapid file events into single reload messages.
///
/// After receiving an event, waits `DEBOUNCE_INTERVAL` for further events. If more
/// events arrive during the wait, the timer resets and the latest event wins.
/// When the timer expires, a single [`ReloadMessage`] is sent.
async fn debounce_loop(
    mut rx: tokio::sync::mpsc::Receiver<RawChangeEvent>,
    tx: Sender<ReloadMessage>,
) {
    loop {
        // Wait for the first event.
        let first = match rx.recv().await {
            Some(e) => e,
            None => return, // Channel closed — shutting down.
        };

        let mut pending = first;

        // Drain any events that arrive within the debounce window.
        let deadline = tokio::time::Instant::now() + DEBOUNCE_INTERVAL;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }

            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(event)) => {
                    pending = event; // Last event wins.
                }
                Ok(None) => return, // Channel closed.
                Err(_) => break,    // Timeout — debounce window expired.
            }
        }

        // Emit the debounced reload message.
        let msg = if pending.is_remove {
            ReloadMessage::FileRemoved { path: pending.path }
        } else {
            ReloadMessage::FileChanged { path: pending.path }
        };

        if tx.send(msg).await.is_err() {
            return; // Receiver dropped — shutting down.
        }
    }
}

/// Check whether a `notify` event involves at least one `.toml` file.
pub fn is_toml_event(event: &Event) -> bool {
    event
        .paths
        .iter()
        .any(|p| p.extension().is_some_and(|ext| ext == "toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_toml_event_with_toml_extension() {
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/workflows/plan.toml")],
            attrs: Default::default(),
        };
        assert!(is_toml_event(&event));
    }

    #[test]
    fn test_is_toml_event_with_non_toml_extension() {
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/workflows/notes.txt")],
            attrs: Default::default(),
        };
        assert!(!is_toml_event(&event));
    }

    #[test]
    fn test_is_toml_event_with_no_extension() {
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/workflows/Dockerfile")],
            attrs: Default::default(),
        };
        assert!(!is_toml_event(&event));
    }

    #[test]
    fn test_is_toml_event_mixed_extensions() {
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![
                PathBuf::from("/workflows/notes.txt"),
                PathBuf::from("/workflows/plan.toml"),
            ],
            attrs: Default::default(),
        };
        assert!(is_toml_event(&event));
    }

    #[test]
    fn test_classify_event_create() {
        let event = Event {
            kind: EventKind::Create(notify::event::CreateKind::File),
            paths: vec![PathBuf::from("/workflows/plan.toml")],
            attrs: Default::default(),
        };
        let change = classify_event(&event).unwrap();
        assert!(!change.is_remove);
        assert_eq!(change.path, PathBuf::from("/workflows/plan.toml"));
    }

    #[test]
    fn test_classify_event_modify() {
        let event = Event {
            kind: EventKind::Modify(notify::event::ModifyKind::Any),
            paths: vec![PathBuf::from("/workflows/plan.toml")],
            attrs: Default::default(),
        };
        let change = classify_event(&event).unwrap();
        assert!(!change.is_remove);
    }

    #[test]
    fn test_classify_event_remove() {
        let event = Event {
            kind: EventKind::Remove(notify::event::RemoveKind::File),
            paths: vec![PathBuf::from("/workflows/old.toml")],
            attrs: Default::default(),
        };
        let change = classify_event(&event).unwrap();
        assert!(change.is_remove);
    }

    #[test]
    fn test_classify_event_access_ignored() {
        let event = Event {
            kind: EventKind::Access(notify::event::AccessKind::Any),
            paths: vec![PathBuf::from("/workflows/plan.toml")],
            attrs: Default::default(),
        };
        assert!(classify_event(&event).is_none());
    }

    #[tokio::test]
    async fn test_debounce_loop_single_event() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<RawChangeEvent>(32);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<ReloadMessage>(32);

        tokio::spawn(debounce_loop(raw_rx, out_tx));

        raw_tx
            .send(RawChangeEvent {
                is_remove: false,
                path: PathBuf::from("plan.toml"),
            })
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");

        match msg {
            ReloadMessage::FileChanged { path } => assert_eq!(path, PathBuf::from("plan.toml")),
            ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged"),
        }
    }

    #[tokio::test]
    async fn test_debounce_loop_rapid_events() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<RawChangeEvent>(32);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<ReloadMessage>(32);

        tokio::spawn(debounce_loop(raw_rx, out_tx));

        // Send 3 rapid events.
        for i in 0..3 {
            raw_tx
                .send(RawChangeEvent {
                    is_remove: false,
                    path: PathBuf::from(format!("plan{i}.toml")),
                })
                .await
                .unwrap();
        }

        // Should get exactly one debounced output.
        let msg = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        match msg {
            ReloadMessage::FileChanged { path } => {
                assert_eq!(path, PathBuf::from("plan2.toml")); // Last event wins.
            }
            ReloadMessage::FileRemoved { .. } => panic!("expected FileChanged"),
        }

        // No second event should arrive.
        let result = tokio::time::timeout(Duration::from_millis(800), out_rx.recv()).await;
        assert!(result.is_err(), "expected only one debounced event");
    }

    #[tokio::test]
    async fn test_debounce_loop_remove_event() {
        let (raw_tx, raw_rx) = tokio::sync::mpsc::channel::<RawChangeEvent>(32);
        let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<ReloadMessage>(32);

        tokio::spawn(debounce_loop(raw_rx, out_tx));

        raw_tx
            .send(RawChangeEvent {
                is_remove: true,
                path: PathBuf::from("gone.toml"),
            })
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
        match msg {
            ReloadMessage::FileRemoved { path } => {
                assert_eq!(path, PathBuf::from("gone.toml"));
            }
            ReloadMessage::FileChanged { .. } => panic!("expected FileRemoved"),
        }
    }
}
