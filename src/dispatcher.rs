//! Dispatcher: concurrency control, deduplication, and persistence.
//!
//! This module provides two core mechanisms:
//!
//! 1. **Concurrency limiting** — A `Dispatcher` wraps an optional `tokio::Semaphore`
//!    that caps how many workflows can run simultaneously. When `max_concurrent`
//!    is 0 (unlimited), the semaphore is `None` and every event starts immediately.
//!    When `max_concurrent > 0`, the dispatcher acquires a permit before spawning
//!    each workflow, and the permit is automatically released when the workflow
//!    completes (success or failure) via the RAII guard pattern.
//!
//! 2. **Deduplication** — Three `HashSet`s track event lifecycle states
//!    (`in_flight`, `completed`, `permanently_failed`) to prevent concurrent
//!    or repeated execution of the same webhook event. The dedup key format
//!    is `{owner}/{repo}/{event_id}`, where `event_id` varies by event type:
//!    - Issue events: the issue number
//!    - PR review events: `{pr_number}_review-{review_id}`
//!    - Issue comment events: the issue number
//!    - PR review comment events: `{pr_number}_comment-{comment_id}`
//!
//! Thread-safe access is provided via `SharedDedupSets` (`Arc<RwLock<DedupSets>>`).
//!
//! Persistence uses atomic file writes (write to `.tmp`, then `rename`) to
//! prevent data corruption on crash. On startup, `load_persistence` reads
//! `completed.json` and `failed.json` from the work directory, gracefully
//! handling missing or corrupted files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, RwLock, Semaphore};

use crate::config::AgentConfig;
use crate::git;
use crate::reload::WorkflowState;
use crate::runner::WorkflowRunner;
use crate::webhook::TriggerEvent;
use crate::workflow::{TriggerType, Workflow};
use tracing::instrument;

/// A record of a permanently failed event, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FailedEntry {
    /// The dedup key of the failed event (e.g. `owner/repo/42`).
    pub key: String,
    /// When the failure occurred.
    pub timestamp: SystemTime,
    /// Description of the error that caused the failure.
    pub error: String,
}

/// Last-processed watermark per repository.
///
/// Persists the timestamp and platform-specific event/delivery ID of the
/// most recently processed webhook event for each repo. This enables
/// catch-up orchestration: on restart or after downtime, Yoke can query
/// the platform APIs for events newer than the watermark to process
/// anything that was missed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Watermark {
    /// GitHub delivery ID (from `X-GitHub-Delivery` header).
    pub last_delivery_id: Option<String>,
    /// GitLab event ID (from GitLab webhook payload).
    pub last_event_id: Option<String>,
    /// When Yoke last processed an event for this repo.
    pub last_processed_at: DateTime<Utc>,
}

/// Wrapper for the watermark map, keyed by `{owner}/{repo}`.
///
/// Follows the same atomic persistence pattern as `DedupSets`:
/// write to `.tmp`, then `rename` for crash safety.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WatermarkStore {
    pub marks: HashMap<String, Watermark>,
}

/// Message sent from the webhook handler to the dispatcher loop.
///
/// Each `DispatchMessage` carries a verified `TriggerEvent` that needs
/// workflow processing. The dispatcher consumes these from an mpsc channel
/// and spawns a workflow runner for each non-duplicate event.
#[derive(Debug, Clone)]
pub struct DispatchMessage {
    /// The verified trigger event to process.
    pub event: TriggerEvent,
}

/// Result of a completed workflow run, sent back to the dispatcher
/// for state tracking and persistence.
#[derive(Debug)]
pub struct WorkflowResult {
    /// The dedup key of the completed event.
    pub key: String,
    /// Whether the workflow completed successfully.
    pub success: bool,
    /// Error message if the workflow failed.
    pub error: Option<String>,
}

/// Errors that can occur during persistence operations.
#[derive(Debug, Error)]
pub enum PersistenceError {
    /// An I/O error occurred reading or writing a file.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// A JSON serialization/deserialization error occurred.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Three-set deduplication tracker for webhook events.
///
/// Events transition through the following states:
/// 1. `in_flight` — event is currently being processed
/// 2. `completed` — event finished successfully (terminal state)
/// 3. `permanently_failed` — event failed and will not be retried (terminal state)
///
/// An event is considered a duplicate if its key exists in any of the three sets.
#[derive(Debug, Clone, Default)]
pub struct DedupSets {
    pub in_flight: HashSet<String>,
    pub completed: HashSet<String>,
    pub permanently_failed: HashSet<String>,
}

impl DedupSets {
    /// Create a new empty `DedupSets`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Check whether a key exists in any of the three dedup sets.
    ///
    /// Returns `true` if the event is currently being processed, has already
    /// completed, or has permanently failed — i.e. it should not be processed again.
    pub fn is_duplicate(&self, key: &str) -> bool {
        self.in_flight.contains(key)
            || self.completed.contains(key)
            || self.permanently_failed.contains(key)
    }

    /// Mark an event as in-flight (currently being processed).
    ///
    /// Call this before starting event processing to prevent concurrent execution.
    pub fn mark_in_flight(&mut self, key: &str) {
        self.in_flight.insert(key.to_string());
    }

    /// Move an event from `in_flight` to `completed`.
    ///
    /// Call this when event processing finishes successfully.
    pub fn mark_completed(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.completed.insert(key.to_string());
    }

    /// Move an event from `in_flight` to `permanently_failed`.
    ///
    /// Call this when event processing fails and should not be retried.
    pub fn mark_failed(&mut self, key: &str) {
        self.in_flight.remove(key);
        self.permanently_failed.insert(key.to_string());
    }

    /// Remove an event from `in_flight` without moving it to a terminal state.
    ///
    /// Use this to clean up in-flight tracking when the event should be
    /// re-processed (e.g. a transient failure that allows retries).
    pub fn remove_in_flight(&mut self, key: &str) {
        self.in_flight.remove(key);
    }
}

/// Thread-safe wrapper around `DedupSets` using `Arc<RwLock<...>>`.
///
/// This type is `Clone` (cheaply, via `Arc` clone) and can be shared across
/// async tasks. Use `read()` for `is_duplicate` checks and `write()` for
/// state transitions.
pub type SharedDedupSets = Arc<RwLock<DedupSets>>;

// ---------------------------------------------------------------------------
// Dispatcher: concurrency control via tokio::sync::Semaphore
// ---------------------------------------------------------------------------

/// Dispatcher coordinates concurrency limiting, deduplication, and workflow
/// spawning for webhook event processing.
///
/// The dispatcher runs as a single-consumer loop in a dedicated tokio task.
/// It receives `DispatchMessage`s from an mpsc channel, performs dedup checks,
/// acquires concurrency permits, and spawns workflow runners as independent
/// tokio tasks. Completed workflows transition through dedup state tracking
/// and are persisted to disk.
///
/// When `max_concurrent > 0`, the dispatcher holds a `Semaphore` that caps
/// how many workflows can execute simultaneously. When `max_concurrent == 0`,
/// the semaphore is `None` and no limiting is applied — every event starts
/// immediately.
#[derive(Clone)]
pub struct Dispatcher {
    /// Shared deduplication state for tracking event lifecycles.
    pub dedup_sets: SharedDedupSets,
    /// Last-processed watermark per repository, persisted to `watermark.json`.
    pub watermark_store: Arc<RwLock<WatermarkStore>>,
    /// Optional concurrency-limiting semaphore. `None` means unlimited.
    semaphore: Option<Arc<Semaphore>>,
    /// Counter for permits currently held (for observability).
    active_count: Arc<AtomicUsize>,
    /// The maximum concurrent workflows (0 = unlimited). Stores the value
    /// for logging; the actual limiting is done by the semaphore.
    max_concurrent: usize,
    /// Directory for persisting completed/failed dedup state.
    workdir: PathBuf,
    /// Hot-reloadable workflow state for matching events to workflows.
    workflow_state: Arc<WorkflowState>,
    /// Agent configurations for constructing HermesClient instances.
    agents: Vec<AgentConfig>,
    /// Whether the dispatcher is shutting down (set by shutdown signal).
    shutting_down: Arc<AtomicBool>,
    /// GitLab host URL for building clone URLs (e.g. "gitlab.com").
    /// Required for GitLab platform repos; unused for GitHub.
    gitlab_host: Option<String>,
}

impl Dispatcher {
    /// Create a new `Dispatcher`.
    ///
    /// If `max_concurrent` is 0, no semaphore is created and concurrency is
    /// unlimited. Otherwise, a semaphore with `max_concurrent` permits is
    /// allocated. The `workdir` path is used for persisting completed/failed
    /// dedup state to disk.
    ///
    /// The `workflow_state` provides hot-reloadable workflow definitions used
    /// to match incoming events to workflow configurations. The `agents` list
    /// provides the Hermes API configurations for constructing clients.
    pub fn new(
        dedup_sets: SharedDedupSets,
        watermark_store: Arc<RwLock<WatermarkStore>>,
        max_concurrent: usize,
        workdir: PathBuf,
        workflow_state: Arc<WorkflowState>,
        agents: Vec<AgentConfig>,
        gitlab_host: Option<String>,
    ) -> Self {
        let semaphore = if max_concurrent > 0 {
            Some(Arc::new(Semaphore::new(max_concurrent)))
        } else {
            None
        };

        tracing::debug!(
            max_concurrent,
            "Dispatcher initialized ({})",
            if max_concurrent == 0 {
                "unlimited"
            } else {
                "concurrency limited"
            }
        );

        Self {
            dedup_sets,
            watermark_store,
            semaphore,
            active_count: Arc::new(AtomicUsize::new(0)),
            max_concurrent,
            workdir,
            workflow_state,
            agents,
            shutting_down: Arc::new(AtomicBool::new(false)),
            gitlab_host,
        }
    }

    /// Check whether the dispatcher is shutting down.
    ///
    /// Returns `true` after `mark_shutting_down()` has been called,
    /// which happens when a shutdown signal is received.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Relaxed)
    }

    /// Mark the dispatcher as shutting down.
    ///
    /// Called when a shutdown signal is received. After this, `is_shutting_down()`
    /// returns `true` and the `/ready` endpoint will return 503.
    pub fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
    }

    /// Ensure a base clone of the repository exists and is up to date.
    ///
    /// The base repository is cloned to `{workdir}/repos/{owner}/{repo}` — a
    /// shared, long-lived clone that serves as the parent for worktrees.
    /// If the base repo already exists, it is fetched and fast-forwarded to
    /// the remote's default branch. If it doesn't exist, it is cloned.
    ///
    /// Returns the path to the base repository on success.
    fn ensure_base_repo(
        &self,
        owner: &str,
        repo: &str,
        platform: &str,
        token: &str,
    ) -> Result<PathBuf, git::GitError> {
        let base_path = base_repo_path(&self.workdir, owner, repo);

        if base_path.exists() {
            tracing::debug!(
                owner,
                repo,
                path = %base_path.display(),
                "Base repo exists, pulling latest"
            );
            let git_repo = git2::Repository::open(&base_path).map_err(git::GitError::Git)?;
            git::pull_repo(&git_repo, token, platform)?;
        } else {
            tracing::debug!(
                owner,
                repo,
                path = %base_path.display(),
                "Base repo not found, cloning"
            );
            // Create parent directories before cloning
            if let Some(parent) = base_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let url = git::build_clone_url(platform, owner, repo, self.gitlab_host.as_deref());
            git::clone_repo(&url, &base_path, token, platform)?;
        }

        Ok(base_path)
    }

    /// Acquire a concurrency permit from the semaphore.
    ///
    /// Returns `Ok(Some(permit))` when concurrency is limited — the permit
    /// must be held for the duration of the workflow and is released
    /// automatically when dropped.
    ///
    /// Returns `Ok(None)` when concurrency is unlimited (`max_concurrent == 0`).
    ///
    /// Returns `Err` if the semaphore is closed (should not happen in normal
    /// operation).
    pub async fn acquire_permit(
        &self,
    ) -> Result<Option<OwnedSemaphorePermit>, tokio::sync::AcquireError> {
        match &self.semaphore {
            Some(sem) => {
                tracing::debug!(
                    active = self.active_count.load(Ordering::Relaxed),
                    max = self.max_concurrent,
                    "acquiring concurrency permit"
                );
                let permit = sem.clone().acquire_owned().await?;
                self.active_count.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    active = self.active_count.load(Ordering::Relaxed),
                    max = self.max_concurrent,
                    "concurrency permit acquired"
                );
                Ok(Some(permit))
            }
            None => Ok(None),
        }
    }

    /// Create a wrapper future that holds the permit for the duration of
    /// the inner future, releasing it automatically on completion (success
    /// or failure).
    ///
    /// If concurrency is unlimited (`max_concurrent == 0`), the inner future
    /// runs directly without any permit management.
    pub async fn run_with_permit<F>(&self, fut: F) -> F::Output
    where
        F: std::future::Future,
    {
        let permit = self.acquire_permit().await.ok().flatten();
        let result = fut.await;
        if permit.is_some() {
            self.active_count.fetch_sub(1, Ordering::Relaxed);
        }
        drop(permit);
        result
    }

    /// Returns the configured maximum concurrent workflows (0 = unlimited).
    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Returns the number of currently active (held) permits.
    pub fn active_count(&self) -> usize {
        self.active_count.load(Ordering::Relaxed)
    }

    /// Returns a reference to the workdir path.
    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    /// Run the dispatcher loop with a configurable drain timeout.
    ///
    /// Consumes `DispatchMessage`s from the provided `mpsc::Receiver`, performs
    /// dedup checks, acquires concurrency permits, and spawns workflow runners
    /// as independent tokio tasks.
    ///
    /// When a `shutdown` signal is received (the watch value becomes `true`),
    /// the loop stops consuming new messages and waits for in-flight
    /// workflows to complete (up to `drain_timeout`) before persisting state
    /// and returning.
    ///
    /// # Arguments
    ///
    /// * `rx` — The receiving end of the mpsc channel for dispatch messages.
    /// * `shutdown` — A `watch::Receiver<bool>` that signals graceful shutdown
    ///   when the value becomes `true`.
    /// * `drain_timeout` — Maximum duration to wait for in-flight workflows
    ///   to complete before giving up.
    pub async fn run_with_drain(
        &self,
        mut rx: tokio::sync::mpsc::Receiver<DispatchMessage>,
        shutdown: &mut tokio::sync::watch::Receiver<bool>,
        drain_timeout: Duration,
    ) {
        tracing::info!(?drain_timeout, "Dispatcher run loop started");

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Some(dispatch_msg) => {
                            self.spawn_workflow(dispatch_msg).await;
                        }
                        None => {
                            tracing::info!("Dispatcher channel closed, stopping");
                            break;
                        }
                    }
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        tracing::info!("Dispatcher received shutdown signal, draining in-flight tasks");
                        break;
                    }
                }
            }
        }

        // Drain in-flight tasks: wait until active_count reaches 0 or timeout
        let drain_interval = Duration::from_millis(100);
        let mut elapsed = Duration::ZERO;

        while self.active_count() > 0 && elapsed < drain_timeout {
            tracing::info!(
                active = self.active_count(),
                "Waiting for in-flight workflows to complete"
            );
            tokio::time::sleep(drain_interval).await;
            elapsed += drain_interval;
        }

        if self.active_count() > 0 {
            tracing::warn!(
                active = self.active_count(),
                "Shutdown timed out, some in-flight workflows may not have completed"
            );
        } else {
            tracing::info!("All in-flight workflows completed, dispatcher shut down");
        }

        // Persist state before exit
        self.persist_state().await;
    }

    /// Persist the current state of completed and failed sets to disk.
    ///
    /// Called during graceful shutdown to ensure state is saved before exit,
    /// even if no workflow just completed. Uses the same atomic write
    /// pattern as individual persist calls.
    async fn persist_state(&self) {
        tracing::info!("Persisting dispatcher state before exit");
        let sets = self.dedup_sets.read().await;
        let completed_path = self.workdir.join("completed.json");
        if let Err(e) = sets.persist_completed(&self.workdir) {
            tracing::error!(
                error = %e,
                path = %completed_path.display(),
                "Failed to persist completed set during shutdown"
            );
        }
        // persist_failed only appends individual entries; we re-write the
        // permanent_failed keys as a safety net by writing the full file
        if !sets.permanently_failed.is_empty() {
            let path = self.workdir.join("failed.json");
            // Load existing entries and rewrite (or start fresh)
            let mut failed_entries: Vec<FailedEntry> = load_dedup_file(&path).unwrap_or_default();
            // Ensure all currently tracked permanently_failed keys are present
            let existing_keys: HashSet<String> =
                failed_entries.iter().map(|e| e.key.clone()).collect();
            for key in &sets.permanently_failed {
                if !existing_keys.contains(key) {
                    failed_entries.push(FailedEntry {
                        key: key.clone(),
                        timestamp: SystemTime::now(),
                        error: "persisted during shutdown".to_string(),
                    });
                }
            }
            if let Err(e) = save_dedup_file(&path, &failed_entries) {
                tracing::error!(
                    error = %e,
                    path = %path.display(),
                    "Failed to persist failed set during shutdown"
                );
            }
        }
        drop(sets);

        // Persist watermarks
        let wm = self.watermark_store.read().await;
        if let Err(e) = wm.persist(&self.workdir) {
            tracing::error!(error = %e, "Failed to persist watermarks during shutdown");
        }
    }

    /// Process a single dispatch message: dedup check, permit acquisition,
    /// and workflow task spawn.
    ///
    /// If the event is a duplicate (already in flight, completed, or failed),
    /// it is skipped with a warning. Otherwise, it is marked in-flight, a
    /// concurrency permit is acquired (if limiting is enabled), and a tokio
    /// task is spawned to run the workflow. On completion, the dedup state is
    /// updated and persisted to disk.
    #[instrument(skip_all, fields(event_id = %msg.event.event_id, repo = %msg.event.repo_path))]
    async fn spawn_workflow(&self, msg: DispatchMessage) {
        let event = msg.event;
        let event_id = event.event_id.clone();
        let key = build_dedup_key(
            &parse_owner(&event.repo_path),
            &parse_repo(&event.repo_path),
            &event_id,
        );

        // Dedup check (sequential — single consumer, no races)
        {
            let sets = self.dedup_sets.read().await;
            if sets.is_duplicate(&key) {
                tracing::warn!(%key, "Skipping duplicate event");
                return;
            }
        }

        // Mark in-flight
        {
            let mut sets = self.dedup_sets.write().await;
            sets.mark_in_flight(&key);
        }

        // Find matching workflows before git orchestration so we can
        // determine git requirements from workflow config.
        let workflows = self.workflow_state.load();
        let matching = find_matching_workflows(&workflows, &event.trigger_type, &event.actor);

        if matching.is_empty() {
            tracing::warn!(
                trigger = %event.trigger_type,
                repo = %event.repo_path,
                event_id = %event.event_id,
                actor = %event.actor,
                "No matching workflow found for trigger type and actor, skipping"
            );
            // Move the event from in-flight to completed. The event was
            // received and processed by the dispatcher, but no workflow
            // was configured for this trigger/actor combination. Marking
            // as completed (not removing from in-flight) ensures the
            // dedup set correctly records that this event was handled.
            let mut sets = self.dedup_sets.write().await;
            sets.mark_completed(&key);
            if let Err(e) = sets.persist_completed(&self.workdir) {
                tracing::error!(%key, error = %e, "Failed to persist completed set");
            }
            return;
        }

        // Determine git requirements from matching workflows.
        // If any matching workflow requests git.clone, the base repo is
        // ensured. If any requests git.worktree, a worktree is created.
        let needs_clone = matching.iter().any(|(_, wf)| wf.git.clone);
        let needs_worktree = matching.iter().any(|(_, wf)| wf.git.worktree);
        // Collect the default branch from the first workflow that has
        // worktree enabled (used as fallback when event.branch is None).
        let default_branch = matching
            .iter()
            .find(|(_, wf)| wf.git.worktree)
            .map(|(_, wf)| wf.git.default_branch.clone());

        // Acquire concurrency permit
        let permit = match self.acquire_permit().await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!(%key, error = %e, "Failed to acquire concurrency permit");
                // Remove from in-flight so it can be retried
                let mut sets = self.dedup_sets.write().await;
                sets.remove_in_flight(&key);
                return;
            }
        };

        tracing::info!(%key, "Spawning workflow");

        // Clone what we need for the spawned task
        let dedup_sets = self.dedup_sets.clone();
        let watermark_store = self.watermark_store.clone();
        let workdir = self.workdir.clone();
        let agents = self.agents.clone();
        let active_count = self.active_count.clone();

        // Build the per-event workspace directory
        let owner = parse_owner(&event.repo_path);
        let repo = parse_repo(&event.repo_path);
        let event_ws_dir = workspace_dir(&workdir, &owner, &repo, &event_id);

        // Ensure the workspace directory exists before spawning the task
        if let Err(e) = std::fs::create_dir_all(&event_ws_dir) {
            tracing::error!(%key, path = %event_ws_dir.display(), error = %e, "Failed to create workspace directory");
            // Remove from in-flight so it can be retried
            let mut sets = self.dedup_sets.write().await;
            sets.remove_in_flight(&key);
            // Decrement active_count since we acquired a permit but won't spawn a task
            if permit.is_some() {
                self.active_count.fetch_sub(1, Ordering::Relaxed);
            }
            return;
        }

        // Git orchestration: when any matching workflow requests git.clone
        // or git.worktree, ensure the base repo is cloned and optionally
        // create a worktree at the event workspace directory.
        // This pre-initializes the workspace so the agent has access to
        // the repository source code.
        if needs_clone || needs_worktree {
            let platform_str = match event.trigger_type.platform() {
                Some(crate::config::Platform::Github) => "github",
                Some(crate::config::Platform::Gitlab) => "gitlab",
                None => {
                    tracing::error!(
                        %key,
                        "Cannot determine platform for git orchestration — no platform set"
                    );
                    let mut sets = self.dedup_sets.write().await;
                    sets.mark_failed(&key);
                    if permit.is_some() {
                        self.active_count.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            };

            let token_env = match platform_str {
                "gitlab" => crate::config::env::GITLAB_TOKEN,
                _ => crate::config::env::GITHUB_TOKEN,
            };
            let token = match std::env::var(token_env) {
                Ok(t) => t,
                Err(_) => {
                    tracing::error!(
                        %key,
                        env = token_env,
                        "Missing git token for git orchestration"
                    );
                    let mut sets = self.dedup_sets.write().await;
                    sets.mark_failed(&key);
                    if permit.is_some() {
                        self.active_count.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            };

            match self.ensure_base_repo(&owner, &repo, platform_str, &token) {
                Ok(base_path) => {
                    if needs_worktree {
                        // Resolve the branch: prefer the webhook-provided branch,
                        // fall back to the workflow's configured default_branch.
                        let branch = event.branch.clone().or(default_branch);
                        let branch = match branch {
                            Some(b) => b,
                            None => {
                                tracing::error!(
                                    %key,
                                    "No branch available for worktree — event has no branch \
                                     and no workflow default_branch is set"
                                );
                                let mut sets = self.dedup_sets.write().await;
                                sets.mark_failed(&key);
                                if permit.is_some() {
                                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                                }
                                return;
                            }
                        };

                        let worktree_dir = event_ws_dir.join("worktree");
                        tracing::info!(
                            %key,
                            branch = %branch,
                            base = %base_path.display(),
                            worktree = %worktree_dir.display(),
                            "Creating worktree for git orchestration"
                        );

                        match git2::Repository::open(&base_path) {
                            Ok(base_repo) => {
                                if let Err(e) =
                                    git::create_worktree(&base_repo, &branch, &worktree_dir)
                                {
                                    tracing::error!(
                                        %key,
                                        branch = %branch,
                                        error = %e,
                                        "Failed to create worktree"
                                    );
                                    let mut sets = self.dedup_sets.write().await;
                                    sets.mark_failed(&key);
                                    if permit.is_some() {
                                        self.active_count.fetch_sub(1, Ordering::Relaxed);
                                    }
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::error!(
                                    %key,
                                    path = %base_path.display(),
                                    error = %e,
                                    "Failed to open base repo for worktree creation"
                                );
                                let mut sets = self.dedup_sets.write().await;
                                sets.mark_failed(&key);
                                if permit.is_some() {
                                    self.active_count.fetch_sub(1, Ordering::Relaxed);
                                }
                                return;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::error!(
                        %key,
                        owner = %owner,
                        repo = %repo,
                        error = %e,
                        "Failed to ensure base repo for git orchestration"
                    );
                    let mut sets = self.dedup_sets.write().await;
                    sets.mark_failed(&key);
                    if permit.is_some() {
                        self.active_count.fetch_sub(1, Ordering::Relaxed);
                    }
                    return;
                }
            }
        }

        tokio::spawn(async move {
            let result = async {
                tracing::info!(
                    trigger = %event.trigger_type,
                    repo = %event.repo_path,
                    event_id = %event.event_id,
                    workspace = %event_ws_dir.display(),
                    "Processing workflow event"
                );

                // Read the Hermes API key from the environment
                let api_key = std::env::var(crate::config::env::HERMES_API_KEY)
                    .map_err(|_| "HERMES_API_KEY environment variable not set".to_string())?;

                // Run each matching workflow sequentially within this task.
                // If multiple workflows match the same trigger, they run one after
                // another in the same workspace directory.
                for (path, workflow) in matching {
                    let workflow_name = std::path::Path::new(&path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy();
                    tracing::info!(
                        workflow = %workflow_name,
                        steps = workflow.steps.len(),
                        repo = %event.repo_path,
                        event_id = %event_id,
                        "Running workflow"
                    );

                    // Build template variables from the event context
                    // Start with trigger-specific variables (from the webhook payload),
                    // then overlay global context variables. Global keys take precedence
                    // over trigger-specific keys to avoid accidental shadowing of
                    // well-known variables like `owner` and `repo`.
                    let mut variables = event.variables.clone();
                    variables.insert("owner".to_string(), owner.clone());
                    variables.insert("repo".to_string(), repo.clone());
                    variables.insert("output_dir".to_string(), event_ws_dir.display().to_string());
                    variables.insert("event_id".to_string(), event_id.clone());
                    variables.insert("repo_path".to_string(), event.repo_path.clone());
                    if let Some(ref branch) = event.branch {
                        variables.insert("branch".to_string(), branch.clone());
                    }

                    let mut runner = WorkflowRunner::new(
                        workflow.clone(),
                        variables,
                        event_ws_dir.clone(),
                        agents.clone(),
                        api_key.clone(),
                    );

                    if let Err(e) = runner.run().await {
                        tracing::error!(
                            workflow = %workflow_name,
                            repo = %event.repo_path,
                            event_id = %event_id,
                            error = %e,
                            "Workflow execution failed"
                        );
                        return Err(format!("Workflow '{}' failed: {}", path, e));
                    }

                    tracing::info!(
                        workflow = %workflow_name,
                        repo = %event.repo_path,
                        event_id = %event_id,
                        "Workflow completed successfully"
                    );
                }

                Ok::<(), String>(())
            }
            .await;

            // Update dedup state and persist
            let succeeded = result.is_ok();
            complete_workflow(&dedup_sets, &workdir, &key, result).await;

            // Update watermark for this repo on success
            if succeeded {
                let repo_key = format!("{}/{}", owner, repo);
                let mut wm_store = watermark_store.write().await;
                wm_store.marks.insert(
                    repo_key,
                    Watermark {
                        last_delivery_id: event.delivery_id.clone(),
                        last_event_id: Some(event.event_id.clone()),
                        last_processed_at: Utc::now(),
                    },
                );
                if let Err(e) = wm_store.persist(&workdir) {
                    tracing::error!(%key, error = %e, "Failed to persist watermarks");
                }
            }
            // Decrement active_count (incremented by acquire_permit) now that
            // the workflow task is done, regardless of success or failure.
            if permit.is_some() {
                active_count.fetch_sub(1, Ordering::Relaxed);
            }
            // Permit released on drop (RAII) — releases the semaphore slot
            drop(permit);
        });
    }

    /// Handle the completion of a workflow by updating dedup state and
    /// persisting to disk.
    ///
    /// This method encapsulates the state transition and persistence logic
    /// for a completed workflow. On success, the event key moves from
    /// `in_flight` to `completed`. On failure, it moves to
    /// `permanently_failed`.
    #[instrument(skip(self))]
    pub async fn on_workflow_complete(&self, key: &str, result: Result<(), String>) {
        complete_workflow(&self.dedup_sets, &self.workdir, key, result).await;
    }
}

/// Update dedup state and persist after a workflow completes.
///
/// On success, the key moves from `in_flight` to `completed` and the
/// completed set is persisted. On failure, it moves to `permanently_failed`
/// and the failure entry is persisted.
async fn complete_workflow(
    dedup_sets: &Arc<RwLock<DedupSets>>,
    workdir: &Path,
    key: &str,
    result: Result<(), String>,
) {
    let mut sets = dedup_sets.write().await;
    match result {
        Ok(()) => {
            sets.mark_completed(key);
            let completed_path = workdir.join("completed.json");
            if let Err(e) = sets.persist_completed(workdir) {
                tracing::error!(%key, error = %e, path = %completed_path.display(), "Failed to persist completed set");
            }
        }
        Err(e) => {
            sets.mark_failed(key);
            let failed_path = workdir.join("failed.json");
            if let Err(persist_err) = sets.persist_failed(
                workdir,
                &FailedEntry {
                    key: key.to_string(),
                    timestamp: SystemTime::now(),
                    error: e,
                },
            ) {
                tracing::error!(%key, error = %persist_err, path = %failed_path.display(), "Failed to persist failed set");
            }
        }
    }
}

/// Build a dedup key from owner, repo, and event ID.
///
/// The format is `{owner}/{repo}/{event_id}`, where `event_id` is the canonical
/// form defined per trigger type in the architecture design (e.g. `issue-42`,
/// `pr-7-review-999`, `mr-12-comment-250`).
pub fn build_dedup_key(owner: &str, repo: &str, event_id: &str) -> String {
    format!("{}/{}/{}", owner, repo, event_id)
}

/// Parse the owner part from a `repo_path` string (e.g. `"owner/repo"` → `"owner"`).
///
/// Returns the full string if no `/` separator is found.
pub fn parse_owner(repo_path: &str) -> String {
    repo_path
        .split_once('/')
        .map_or(repo_path.to_string(), |(owner, _)| owner.to_string())
}

/// Parse the repo part from a `repo_path` string (e.g. `"owner/repo"` → `"repo"`).
///
/// Returns the full string if no `/` separator is found.
pub fn parse_repo(repo_path: &str) -> String {
    repo_path
        .split_once('/')
        .map_or(repo_path.to_string(), |(_, repo)| repo.to_string())
}

/// Create a new `SharedDedupSets` (wrapped in `Arc<RwLock<...>>`).
pub fn new_dedup_sets() -> SharedDedupSets {
    Arc::new(RwLock::new(DedupSets::new()))
}

/// Build the per-event workspace directory path.
///
/// The format is `{workdir}/{owner}/{repo}/{event_id}/`, matching the
/// data directory layout from the architecture design doc (Section 11).
pub fn workspace_dir(workdir: &Path, owner: &str, repo: &str, event_id: &str) -> PathBuf {
    workdir.join(owner).join(repo).join(event_id)
}

/// Build the base repository clone path.
///
/// The format is `{workdir}/repos/{owner}/{repo}/`. This is a shared,
/// long-lived clone that serves as the parent for per-event worktrees.
pub fn base_repo_path(workdir: &Path, owner: &str, repo: &str) -> PathBuf {
    workdir.join("repos").join(owner).join(repo)
}

// ---------------------------------------------------------------------------
// Persistence: loading and saving dedup sets to JSON files
// ---------------------------------------------------------------------------

/// Load and deserialize a JSON dedup file.
///
/// Returns `Err(PersistenceError::Io(NotFound))` if the file does not exist.
/// Returns `Err(PersistenceError::Json(_))` if the file contains invalid JSON.
fn load_dedup_file<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, PersistenceError> {
    if !path.exists() {
        return Err(PersistenceError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("File not found: {}", path.display()),
        )));
    }
    let content = std::fs::read_to_string(path)?;
    serde_json::from_str(&content).map_err(PersistenceError::Json)
}

/// Save data to a JSON file using atomic writes.
///
/// Writes the data to a `.tmp` file first, then renames it to the target path.
/// The rename operation is atomic on most filesystems, preventing partial writes
/// on crash.
fn save_dedup_file<T: Serialize>(path: &Path, entries: &T) -> Result<(), PersistenceError> {
    let tmp_path = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(entries)?;
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)?;
    Ok(())
}

impl DedupSets {
    /// Persist the `completed` set to `completed.json` in the given directory.
    ///
    /// Creates the directory if it doesn't exist. Uses atomic file writes
    /// (`.tmp` + `rename`) to prevent data corruption.
    pub fn persist_completed(&self, workdir: &Path) -> Result<(), PersistenceError> {
        std::fs::create_dir_all(workdir).map_err(PersistenceError::Io)?;
        let path = workdir.join("completed.json");
        save_dedup_file(&path, &self.completed)
    }

    /// Append a failed entry to `failed.json` in the given directory.
    ///
    /// Creates the directory if it doesn't exist. Loads existing failed entries,
    /// appends the new one, and atomically rewrites the file. On missing or
    /// corrupted `failed.json`, starts with an empty list.
    pub fn persist_failed(
        &self,
        workdir: &Path,
        entry: &FailedEntry,
    ) -> Result<(), PersistenceError> {
        std::fs::create_dir_all(workdir).map_err(PersistenceError::Io)?;
        let path = workdir.join("failed.json");
        let mut failed: Vec<FailedEntry> = load_dedup_file(&path).unwrap_or_default();
        failed.push(entry.clone());
        save_dedup_file(&path, &failed)
    }
}

impl WatermarkStore {
    /// Persist the watermark map to `watermark.json` in the given directory.
    ///
    /// Uses atomic file writes (`.tmp` + `rename`) for crash safety,
    /// same pattern as `DedupSets::persist_completed`.
    pub fn persist(&self, workdir: &Path) -> Result<(), PersistenceError> {
        if self.marks.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(workdir).map_err(PersistenceError::Io)?;
        let path = workdir.join("watermark.json");
        save_dedup_file(&path, &self.marks)
    }
}

/// Load dedup persistence state from the work directory.
///
/// Reads `completed.json` and `failed.json` from `workdir`. Missing files are
/// treated as empty sets (no error). Corrupted files produce a warning on
/// stderr and are treated as empty sets. The `in_flight` set is always empty
/// on startup (in-flight state is transient).
pub fn load_persistence(workdir: &Path) -> DedupSets {
    let completed_path = workdir.join("completed.json");
    let failed_path = workdir.join("failed.json");

    let completed = load_dedup_file::<HashSet<String>>(&completed_path).unwrap_or_else(|e| {
        if !matches!(
            &e,
            PersistenceError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
        ) {
            tracing::warn!(error = %e, "Corrupted completed.json, treating as empty; data will be rebuilt");
        }
        HashSet::new()
    });

    let failed_entries = load_dedup_file::<Vec<FailedEntry>>(&failed_path).unwrap_or_else(|e| {
        if !matches!(
            &e,
            PersistenceError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
        ) {
            tracing::warn!(error = %e, "Corrupted failed.json, treating as empty; data will be rebuilt");
        }
        Vec::new()
    });

    let permanently_failed = failed_entries.into_iter().map(|e| e.key).collect();

    DedupSets {
        in_flight: HashSet::new(),
        completed,
        permanently_failed,
    }
}

/// Load watermark persistence state from the work directory.
///
/// Reads `watermark.json` from `workdir`. Missing files are treated as
/// empty (no error). Corrupted files produce a warning and are treated
/// as empty. Returns a `WatermarkStore` ready for wrapping in `Arc<RwLock<...>>`.
pub fn load_watermarks(workdir: &Path) -> WatermarkStore {
    let watermark_path = workdir.join("watermark.json");
    let marks = load_dedup_file::<HashMap<String, Watermark>>(&watermark_path).unwrap_or_else(|e| {
        if !matches!(
            &e,
            PersistenceError::Io(io_err) if io_err.kind() == std::io::ErrorKind::NotFound
        ) {
            tracing::warn!(error = %e, "Corrupted watermark.json, treating as empty; data will be rebuilt");
        }
        HashMap::new()
    });
    WatermarkStore { marks }
}

/// Create a new empty `WatermarkStore` (for first-run or when no persistence file exists).
pub fn new_watermark_store() -> Arc<RwLock<WatermarkStore>> {
    Arc::new(RwLock::new(WatermarkStore::default()))
}

/// Find workflows whose trigger type matches the incoming event's trigger type
/// **and** whose `allowed_users` list includes the event's actor.
///
/// Matching is based on exact comparison between the workflow's `trigger.type`
/// string and `TriggerType::label()`. Additionally, the actor extracted from
/// the webhook payload must be present in the workflow's `trigger.allowed_users`
/// list — this is a security boundary that prevents unauthorized users from
/// triggering workflows.
fn find_matching_workflows(
    workflows: &Arc<Vec<(String, Workflow)>>,
    trigger_type: &TriggerType,
    actor: &str,
) -> Vec<(String, Workflow)> {
    let label = trigger_type.label();
    workflows
        .iter()
        .filter(|(_, wf)| wf.trigger.r#type == label)
        .filter(|(_, wf)| {
            // Authorize: the actor must appear in the workflow's allowed_users list.
            // This is a SECURITY BOUNDARY — without this check, any user could
            // trigger any workflow.
            wf.trigger
                .allowed_users
                .as_ref()
                .is_some_and(|users| users.iter().any(|u| u == actor))
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::TriggerType;

    /// Helper to create a test Dispatcher with an empty WorkflowState and no agents.
    fn test_dispatcher(dedup: SharedDedupSets, max_concurrent: usize) -> Dispatcher {
        let workflow_state = Arc::new(WorkflowState::new(vec![]));
        let watermark_store = new_watermark_store();
        Dispatcher::new(
            dedup,
            watermark_store,
            max_concurrent,
            PathBuf::from("/tmp/yoke-test"),
            workflow_state,
            vec![],
            None,
        )
    }

    // --- DedupSets struct tests ---

    #[test]
    fn test_dedup_sets_new_is_empty() {
        let sets = DedupSets::new();
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    #[test]
    fn test_dedup_sets_default_is_empty() {
        let sets = DedupSets::default();
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    // --- is_duplicate tests ---

    #[test]
    fn test_is_duplicate_empty_sets() {
        let sets = DedupSets::new();
        assert!(!sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_in_flight() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_completed() {
        let mut sets = DedupSets::new();
        sets.mark_completed("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_permanently_failed() {
        let mut sets = DedupSets::new();
        sets.mark_failed("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    #[test]
    fn test_is_duplicate_different_keys() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        assert!(!sets.is_duplicate("owner/repo/43"));
    }

    // --- mark_in_flight tests ---

    #[test]
    fn test_mark_in_flight_adds_to_set() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        assert!(!sets.in_flight.contains(key));
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));
    }

    #[test]
    fn test_mark_in_flight_prevents_concurrent() {
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        // Second call is idempotent
        sets.mark_in_flight("owner/repo/42");
        assert!(sets.is_duplicate("owner/repo/42"));
    }

    // --- mark_completed tests ---

    #[test]
    fn test_mark_completed_moves_from_in_flight() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));
        assert!(!sets.completed.contains(key));

        sets.mark_completed(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(sets.completed.contains(key), "key should be in completed");
        assert!(
            sets.is_duplicate(key),
            "completed key should still be a duplicate"
        );
    }

    #[test]
    fn test_mark_completed_idempotent() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        sets.mark_completed(key);
        sets.mark_completed(key); // second call is a no-op
        assert!(sets.completed.contains(key));
        assert!(!sets.in_flight.contains(key));
    }

    // --- mark_failed tests ---

    #[test]
    fn test_mark_failed_moves_from_in_flight() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);

        sets.mark_failed(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(
            sets.permanently_failed.contains(key),
            "key should be in permanently_failed"
        );
        assert!(
            sets.is_duplicate(key),
            "failed key should still be a duplicate"
        );
    }

    #[test]
    fn test_mark_failed_idempotent() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        sets.mark_failed(key);
        sets.mark_failed(key); // second call is a no-op
        assert!(sets.permanently_failed.contains(key));
    }

    // --- remove_in_flight tests ---

    #[test]
    fn test_remove_in_flight_removes_key() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";
        sets.mark_in_flight(key);
        assert!(sets.in_flight.contains(key));

        sets.remove_in_flight(key);
        assert!(
            !sets.in_flight.contains(key),
            "key should be removed from in_flight"
        );
        assert!(
            !sets.is_duplicate(key),
            "key should no longer be a duplicate"
        );
    }

    #[test]
    fn test_remove_in_flight_nonexistent_is_noop() {
        let mut sets = DedupSets::new();
        sets.remove_in_flight("owner/repo/42");
        // Should not panic
        assert!(sets.in_flight.is_empty());
    }

    // --- Full state transition tests ---

    #[test]
    fn test_full_lifecycle_in_flight_to_completed() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Start: not a duplicate
        assert!(!sets.is_duplicate(key));

        // Mark in-flight
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));
        assert!(sets.in_flight.contains(key));

        // Mark completed
        sets.mark_completed(key);
        assert!(sets.is_duplicate(key));
        assert!(!sets.in_flight.contains(key));
        assert!(sets.completed.contains(key));
    }

    #[test]
    fn test_full_lifecycle_in_flight_to_failed() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Mark in-flight
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));

        // Mark failed
        sets.mark_failed(key);
        assert!(sets.is_duplicate(key));
        assert!(!sets.in_flight.contains(key));
        assert!(sets.permanently_failed.contains(key));
    }

    #[test]
    fn test_in_flight_then_rollback() {
        let mut sets = DedupSets::new();
        let key = "owner/repo/42";

        // Mark in-flight, then roll back (transient failure, allow retry)
        sets.mark_in_flight(key);
        assert!(sets.is_duplicate(key));

        sets.remove_in_flight(key);
        assert!(!sets.is_duplicate(key));
        assert!(sets.in_flight.is_empty());
    }

    // --- build_dedup_key tests ---

    #[test]
    fn test_build_dedup_key_basic() {
        let key = build_dedup_key("owner", "repo", "issue-42");
        assert_eq!(key, "owner/repo/issue-42");
    }

    #[test]
    fn test_build_dedup_key_with_event_id() {
        let key = build_dedup_key("mintybasil", "yoke", "pr-7-review-999");
        assert_eq!(key, "mintybasil/yoke/pr-7-review-999");
    }

    #[test]
    fn test_build_dedup_key_with_namespace() {
        let key = build_dedup_key("internal-team", "backend-service", "issue-42");
        assert_eq!(key, "internal-team/backend-service/issue-42");
    }

    // --- event_id canonical form tests ---
    // event_id is the canonical form defined per trigger type in the architecture
    // design (Appendix A: Trigger Reference). It is used for dedup keys,
    // workspace dirs, template variables, and logs.

    fn make_trigger_event(trigger_type: TriggerType, event_id: &str) -> TriggerEvent {
        TriggerEvent {
            trigger_type,
            repo_path: "owner/repo".to_string(),
            event_id: event_id.to_string(),
            actor: "test-user".to_string(),
            variables: std::collections::HashMap::new(),
            delivery_id: None,
            branch: None,
        }
    }

    #[test]
    fn test_event_id_canonical_github_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned { assigned_to: None },
            "issue-42",
        );
        assert_eq!(event.event_id, "issue-42");
    }

    #[test]
    fn test_event_id_canonical_github_issue_comment() {
        let event = make_trigger_event(
            TriggerType::GithubIssueCommentMention {
                mentioned_user: None,
            },
            "issue-42-comment-12345",
        );
        assert_eq!(event.event_id, "issue-42-comment-12345");
    }

    #[test]
    fn test_event_id_canonical_github_pr_review() {
        let event = make_trigger_event(TriggerType::GithubPullRequestReview, "pr-7-review-999");
        assert_eq!(event.event_id, "pr-7-review-999");
    }

    #[test]
    fn test_event_id_canonical_github_pr_review_comment() {
        let event = make_trigger_event(
            TriggerType::GithubPullRequestCommentMention {
                mentioned_user: None,
            },
            "pr-7-comment-555",
        );
        assert_eq!(event.event_id, "pr-7-comment-555");
    }

    #[test]
    fn test_event_id_canonical_gitlab_issue_assigned() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueAssigned { assigned_to: None },
            "issue-7",
        );
        assert_eq!(event.event_id, "issue-7");
    }

    #[test]
    fn test_event_id_canonical_gitlab_issue_mention() {
        let event = make_trigger_event(
            TriggerType::GitlabIssueMention {
                mentioned_user: None,
            },
            "issue-7-note-99",
        );
        assert_eq!(event.event_id, "issue-7-note-99");
    }

    #[test]
    fn test_event_id_canonical_gitlab_mr_review() {
        let event = make_trigger_event(TriggerType::GitlabMergeRequestReview, "mr-12-review-150");
        assert_eq!(event.event_id, "mr-12-review-150");
    }

    #[test]
    fn test_event_id_canonical_gitlab_mr_comment() {
        let event = make_trigger_event(
            TriggerType::GitlabMergeRequestCommentMention {
                mentioned_user: None,
            },
            "mr-12-comment-250",
        );
        assert_eq!(event.event_id, "mr-12-comment-250");
    }

    // --- New dedup sets helper ---

    #[test]
    fn test_new_dedup_sets() {
        let sets = new_dedup_sets();
        let read_guard = sets.try_read().unwrap();
        assert!(read_guard.in_flight.is_empty());
        assert!(read_guard.completed.is_empty());
        assert!(read_guard.permanently_failed.is_empty());
    }

    // --- Async RwLock integration tests ---

    #[tokio::test]
    async fn test_shared_dedup_sets_async_read_write() {
        let sets = new_dedup_sets();

        // Write
        {
            let mut guard = sets.write().await;
            guard.mark_in_flight("owner/repo/42");
        }

        // Read
        {
            let guard = sets.read().await;
            assert!(guard.is_duplicate("owner/repo/42"));
            assert!(!guard.is_duplicate("owner/repo/43"));
        }

        // Transition
        {
            let mut guard = sets.write().await;
            guard.mark_completed("owner/repo/42");
        }

        // Verify
        {
            let guard = sets.read().await;
            assert!(guard.is_duplicate("owner/repo/42"));
            assert!(guard.completed.contains("owner/repo/42"));
            assert!(!guard.in_flight.contains("owner/repo/42"));
        }
    }

    // --- build_dedup_key + event_id integration ---

    #[test]
    fn test_integration_build_key_with_event_id() {
        let event = make_trigger_event(
            TriggerType::GithubIssueAssigned { assigned_to: None },
            "issue-42",
        );
        let key = build_dedup_key("mintybasil", "yoke", &event.event_id);
        assert_eq!(key, "mintybasil/yoke/issue-42");
    }

    #[test]
    fn test_integration_pr_review_dedup_key() {
        let event = make_trigger_event(TriggerType::GithubPullRequestReview, "pr-7-review-999");
        let key = build_dedup_key("mintybasil", "yoke", &event.event_id);
        assert_eq!(key, "mintybasil/yoke/pr-7-review-999");
    }

    #[test]
    fn test_integration_gitlab_mr_dedup_key() {
        let event = make_trigger_event(TriggerType::GitlabMergeRequestReview, "mr-12-review-150");
        let key = build_dedup_key("internal-team", "backend-service", &event.event_id);
        assert_eq!(key, "internal-team/backend-service/mr-12-review-150");
    }

    // --- Persistence tests ---

    #[test]
    fn test_load_dedup_file_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let result: Result<HashSet<String>, _> = load_dedup_file(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(&err, PersistenceError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
            "Expected NotFound error, got: {err:?}"
        );
    }

    #[test]
    fn test_load_dedup_file_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        let data = serde_json::to_string_pretty(&HashSet::from([
            "owner/repo/42".to_string(),
            "owner/repo/43".to_string(),
        ]))
        .unwrap();
        std::fs::write(&path, data).unwrap();

        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains("owner/repo/42"));
        assert!(loaded.contains("owner/repo/43"));
    }

    #[test]
    fn test_load_dedup_file_corrupted_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");
        std::fs::write(&path, "not valid json{{{").unwrap();

        let result: Result<HashSet<String>, _> = load_dedup_file(&path);
        assert!(result.is_err(), "Expected error for corrupted JSON");
        assert!(
            matches!(result.unwrap_err(), PersistenceError::Json(_)),
            "Expected JSON error"
        );
    }

    #[test]
    fn test_save_dedup_file_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");

        let set: HashSet<String> =
            HashSet::from(["owner/repo/42".to_string(), "owner/repo/99".to_string()]);
        save_dedup_file(&path, &set).unwrap();

        // File should exist
        assert!(path.exists());
        // No .tmp file should remain
        assert!(!path.with_extension("json.tmp").exists());

        // Content should be valid and match
        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded, set);
    }

    #[test]
    fn test_save_dedup_file_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");

        // Save initial set
        let mut set = HashSet::new();
        set.insert("owner/repo/42".to_string());
        save_dedup_file(&path, &set).unwrap();

        // Save updated set
        set.insert("owner/repo/100".to_string());
        save_dedup_file(&path, &set).unwrap();

        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains("owner/repo/42"));
        assert!(loaded.contains("owner/repo/100"));
    }

    #[test]
    fn test_save_dedup_file_write_failure_preserves_original() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("completed.json");

        // Write a valid initial file
        let initial: HashSet<String> = HashSet::from(["owner/repo/42".to_string()]);
        save_dedup_file(&path, &initial).unwrap();
        assert!(path.exists());

        // Make the directory read-only so the next write (to .tmp file) will fail
        // Note: on Linux, removing write permission from the directory prevents
        // creating new files (including the .tmp file), so the original file
        // should remain untouched.
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let updated: HashSet<String> =
            HashSet::from(["owner/repo/42".to_string(), "owner/repo/99".to_string()]);
        // The write should fail because the directory is read-only
        let result = save_dedup_file(&path, &updated);
        assert!(result.is_err(), "writing to read-only dir should fail");

        // Restore permissions for cleanup
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).unwrap();

        // Original file should be untouched
        let loaded: HashSet<String> = load_dedup_file(&path).unwrap();
        assert_eq!(
            loaded.len(),
            1,
            "original file should be preserved on write failure"
        );
        assert!(
            loaded.contains("owner/repo/42"),
            "original contents should be preserved"
        );
    }

    #[test]
    fn test_persist_completed() {
        let dir = tempfile::tempdir().unwrap();
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        sets.mark_completed("owner/repo/42");

        sets.persist_completed(dir.path()).unwrap();

        let loaded: HashSet<String> = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("completed.json")).unwrap(),
        )
        .unwrap();
        assert!(loaded.contains("owner/repo/42"));
    }

    #[test]
    fn test_persist_failed_appends() {
        let dir = tempfile::tempdir().unwrap();
        let sets = DedupSets::new();

        let entry1 = FailedEntry {
            key: "owner/repo/42".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "timeout".to_string(),
        };
        let entry2 = FailedEntry {
            key: "owner/repo/43".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "connection refused".to_string(),
        };

        sets.persist_failed(dir.path(), &entry1).unwrap();
        sets.persist_failed(dir.path(), &entry2).unwrap();

        let loaded: Vec<FailedEntry> =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("failed.json")).unwrap())
                .unwrap();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0], entry1);
        assert_eq!(loaded[1], entry2);
    }

    #[test]
    fn test_load_persistence_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let sets = load_persistence(dir.path());
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.is_empty());
        assert!(sets.permanently_failed.is_empty());
    }

    #[test]
    fn test_load_persistence_valid_files() {
        let dir = tempfile::tempdir().unwrap();

        // Write completed.json
        let completed = HashSet::from(["owner/repo/42".to_string(), "owner/repo/99".to_string()]);
        std::fs::write(
            dir.path().join("completed.json"),
            serde_json::to_string_pretty(&completed).unwrap(),
        )
        .unwrap();

        // Write failed.json
        let failed = vec![FailedEntry {
            key: "owner/repo/7".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "something went wrong".to_string(),
        }];
        std::fs::write(
            dir.path().join("failed.json"),
            serde_json::to_string_pretty(&failed).unwrap(),
        )
        .unwrap();

        let sets = load_persistence(dir.path());
        assert!(sets.in_flight.is_empty());
        assert!(sets.completed.contains("owner/repo/42"));
        assert!(sets.completed.contains("owner/repo/99"));
        assert!(sets.permanently_failed.contains("owner/repo/7"));
        assert_eq!(sets.completed.len(), 2);
        assert_eq!(sets.permanently_failed.len(), 1);
    }

    #[test]
    fn test_load_persistence_corrupted_file_warns() {
        let dir = tempfile::tempdir().unwrap();

        // Write valid completed.json
        let completed: HashSet<String> = HashSet::new();
        std::fs::write(
            dir.path().join("completed.json"),
            serde_json::to_string_pretty(&completed).unwrap(),
        )
        .unwrap();

        // Write corrupted failed.json
        std::fs::write(dir.path().join("failed.json"), "BAD JSON{{").unwrap();

        let sets = load_persistence(dir.path());
        // Corrupted failed.json → empty permanently_failed
        assert!(sets.permanently_failed.is_empty());
        // Valid completed.json → empty but loaded successfully
        assert!(sets.completed.is_empty());
    }

    #[test]
    fn test_load_persistence_corrupted_completed_json() {
        let dir = tempfile::tempdir().unwrap();

        // Write corrupted completed.json
        std::fs::write(dir.path().join("completed.json"), "not valid json{{{").unwrap();

        // Write valid failed.json
        let failed = vec![FailedEntry {
            key: "owner/repo/7".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "something went wrong".to_string(),
        }];
        std::fs::write(
            dir.path().join("failed.json"),
            serde_json::to_string_pretty(&failed).unwrap(),
        )
        .unwrap();

        let sets = load_persistence(dir.path());
        // Corrupted completed.json → empty completed set
        assert!(
            sets.completed.is_empty(),
            "corrupted completed.json should result in empty completed set"
        );
        // Valid failed.json → still loaded correctly
        assert!(sets.permanently_failed.contains("owner/repo/7"));
    }

    #[test]
    fn test_load_persistence_both_files_corrupted() {
        let dir = tempfile::tempdir().unwrap();

        // Write corrupted files
        std::fs::write(dir.path().join("completed.json"), "BAD DATA").unwrap();
        std::fs::write(dir.path().join("failed.json"), "ALSO BAD").unwrap();

        let sets = load_persistence(dir.path());
        assert!(sets.completed.is_empty(), "corrupted completed → empty set");
        assert!(
            sets.permanently_failed.is_empty(),
            "corrupted failed → empty set"
        );
        assert!(sets.in_flight.is_empty());
    }

    #[test]
    fn test_roundtrip_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();

        // Build dedup sets and persist
        let mut sets = DedupSets::new();
        sets.mark_in_flight("owner/repo/42");
        sets.mark_completed("owner/repo/42");
        sets.mark_failed("owner/repo/7");

        sets.persist_completed(dir.path()).unwrap();
        let failed_entry = FailedEntry {
            key: "owner/repo/7".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "permanent failure".to_string(),
        };
        sets.persist_failed(dir.path(), &failed_entry).unwrap();

        // Load back
        let loaded = load_persistence(dir.path());
        assert!(loaded.completed.contains("owner/repo/42"));
        assert!(loaded.permanently_failed.contains("owner/repo/7"));
        assert!(loaded.in_flight.is_empty()); // in_flight is always empty on load
    }

    #[test]
    fn test_persist_completed_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("non_existent_subdir");
        let mut sets = DedupSets::new();
        sets.mark_completed("owner/repo/42");

        // Should succeed even if the workdir directory doesn't exist
        let result = sets.persist_completed(&workdir);
        assert!(
            result.is_ok(),
            "persist_completed should succeed even if workdir is missing: {result:?}"
        );

        // Verify the file was created
        let completed_path = workdir.join("completed.json");
        assert!(completed_path.exists(), "completed.json should be created");
    }

    #[test]
    fn test_persist_failed_missing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("non_existent_subdir");
        let sets = DedupSets::new();
        let entry = FailedEntry {
            key: "owner/repo/42".to_string(),
            timestamp: SystemTime::UNIX_EPOCH,
            error: "test error".to_string(),
        };

        // Should succeed even if the workdir directory doesn't exist
        let result = sets.persist_failed(&workdir, &entry);
        assert!(
            result.is_ok(),
            "persist_failed should succeed even if workdir is missing: {result:?}"
        );

        // Verify the file was created
        let failed_path = workdir.join("failed.json");
        assert!(failed_path.exists(), "failed.json should be created");
    }

    // --- Dispatcher and Semaphore tests ---

    #[test]
    fn test_dispatcher_new_unlimited() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 0);
        assert_eq!(dispatcher.max_concurrent(), 0);
        assert_eq!(dispatcher.active_count(), 0);
    }

    #[test]
    fn test_dispatcher_new_limited() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);
        assert_eq!(dispatcher.max_concurrent(), 2);
        assert_eq!(dispatcher.active_count(), 0);
    }

    #[tokio::test]
    async fn test_acquire_permit_unlimited_returns_none() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 0);
        let permit = dispatcher.acquire_permit().await.unwrap();
        assert!(permit.is_none(), "unlimited mode should return None permit");
    }

    #[tokio::test]
    async fn test_acquire_permit_limited_returns_some() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);
        let permit = dispatcher.acquire_permit().await.unwrap();
        assert!(permit.is_some(), "limited mode should return Some permit");
        assert_eq!(dispatcher.active_count(), 1);
    }

    #[tokio::test]
    async fn test_permit_released_on_drop() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);

        // Acquire a permit via run_with_permit and verify active count
        let active = Arc::new(AtomicUsize::new(0));
        let ac = active.clone();
        let d = dispatcher.clone();
        d.run_with_permit(async move {
            ac.store(1, Ordering::SeqCst);
        })
        .await;
        // After run_with_permit completes, active count should be 0
        assert_eq!(dispatcher.active_count(), 0);
    }

    #[tokio::test]
    async fn test_run_with_permit_unlimited() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 0);

        let result = dispatcher.run_with_permit(async { 42 }).await;
        assert_eq!(result, 42);
        assert_eq!(dispatcher.active_count(), 0);
    }

    #[tokio::test]
    async fn test_run_with_permit_limited() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);

        let result = dispatcher.run_with_permit(async { 42 }).await;
        assert_eq!(result, 42);
        assert_eq!(
            dispatcher.active_count(),
            0,
            "active count should return to 0 after completion"
        );
    }

    #[tokio::test]
    async fn test_semaphore_limits_concurrency() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);

        let active_count = Arc::new(AtomicUsize::new(0));
        let max_observed = Arc::new(AtomicUsize::new(0));
        let mut handles = vec![];

        for _ in 0..5 {
            let d = dispatcher.clone();
            let ac = active_count.clone();
            let mo = max_observed.clone();
            handles.push(tokio::spawn(async move {
                d.run_with_permit(async {
                    let current = ac.fetch_add(1, Ordering::SeqCst) + 1;
                    mo.fetch_max(current, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    ac.fetch_sub(1, Ordering::SeqCst);
                })
                .await
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        // With max_concurrent=2, we should never see more than 2 active at once
        assert!(
            max_observed.load(Ordering::SeqCst) <= 2,
            "max concurrent exceeded: observed {}",
            max_observed.load(Ordering::SeqCst)
        );
        // With 5 tasks and max_concurrent=2, we should have seen at least 2 concurrent at some point
        assert!(
            max_observed.load(Ordering::SeqCst) >= 2,
            "expected at least 2 concurrent, observed {}",
            max_observed.load(Ordering::SeqCst)
        );
        // All tasks should be done, active count should be 0
        assert_eq!(active_count.load(Ordering::SeqCst), 0);
        assert_eq!(dispatcher.active_count(), 0);
    }

    #[test]
    fn test_dispatcher_clone_shares_state() {
        let dedup = new_dedup_sets();
        let dispatcher = test_dispatcher(dedup, 2);
        let clone = dispatcher.clone();

        // Both should share the same max_concurrent
        assert_eq!(dispatcher.max_concurrent(), clone.max_concurrent());
    }

    #[test]
    fn test_parse_owner() {
        assert_eq!(parse_owner("owner/repo"), "owner");
        assert_eq!(parse_owner("org-name/project-name"), "org-name");
        assert_eq!(parse_owner("no_slash"), "no_slash");
        assert_eq!(parse_owner("a/b/c"), "a");
    }

    #[test]
    fn test_parse_repo() {
        assert_eq!(parse_repo("owner/repo"), "repo");
        assert_eq!(parse_repo("org-name/project-name"), "project-name");
        assert_eq!(parse_repo("no_slash"), "no_slash");
        // splitn(2, '/') yields ["a", "b/c"], so parse_repo returns "b/c"
        assert_eq!(parse_repo("a/b/c"), "b/c");
    }

    // --- workspace_dir tests ---

    #[test]
    fn test_workspace_dir_basic() {
        let workdir = PathBuf::from("/tmp/yoke");
        let ws = workspace_dir(&workdir, "mintybasil", "yoke", "issue-42");
        assert_eq!(ws, PathBuf::from("/tmp/yoke/mintybasil/yoke/issue-42"));
    }

    #[test]
    fn test_workspace_dir_with_complex_event_id() {
        let workdir = PathBuf::from("/tmp/yoke");
        let ws = workspace_dir(&workdir, "org-name", "project", "pr-7-review-999");
        assert_eq!(
            ws,
            PathBuf::from("/tmp/yoke/org-name/project/pr-7-review-999")
        );
    }

    // --- Watermark tests ---

    #[test]
    fn test_watermark_store_default_is_empty() {
        let store = WatermarkStore::default();
        assert!(store.marks.is_empty());
    }

    #[test]
    fn test_watermark_store_insert_and_get() {
        let mut store = WatermarkStore::default();
        let watermark = Watermark {
            last_delivery_id: Some("abc-123".to_string()),
            last_event_id: None,
            last_processed_at: Utc::now(),
        };
        store
            .marks
            .insert("mintybasil/yoke".to_string(), watermark.clone());
        assert!(store.marks.contains_key("mintybasil/yoke"));
        let wm = store.marks.get("mintybasil/yoke").unwrap();
        assert_eq!(wm.last_delivery_id, Some("abc-123".to_string()));
        assert!(wm.last_event_id.is_none());
    }

    #[test]
    fn test_watermark_store_persist_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = WatermarkStore::default();
        let now = Utc::now();
        store.marks.insert(
            "owner/repo".to_string(),
            Watermark {
                last_delivery_id: Some("delivery-1".to_string()),
                last_event_id: Some("event-1".to_string()),
                last_processed_at: now,
            },
        );
        store.marks.insert(
            "other/repo".to_string(),
            Watermark {
                last_delivery_id: None,
                last_event_id: Some("event-2".to_string()),
                last_processed_at: now,
            },
        );

        store.persist(dir.path()).unwrap();

        // Load back
        let loaded = load_watermarks(dir.path());
        assert_eq!(loaded.marks.len(), 2);
        let wm1 = loaded.marks.get("owner/repo").unwrap();
        assert_eq!(wm1.last_delivery_id, Some("delivery-1".to_string()));
        assert_eq!(wm1.last_event_id, Some("event-1".to_string()));
        let wm2 = loaded.marks.get("other/repo").unwrap();
        assert!(wm2.last_delivery_id.is_none());
        assert_eq!(wm2.last_event_id, Some("event-2".to_string()));
    }

    #[test]
    fn test_load_watermarks_missing_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        let store = load_watermarks(dir.path());
        assert!(store.marks.is_empty());
    }

    #[test]
    fn test_load_watermarks_corrupted_file_returns_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("watermark.json"), "not valid json{{{{").unwrap();
        let store = load_watermarks(dir.path());
        assert!(store.marks.is_empty());
    }

    #[test]
    fn test_watermark_store_persist_empty_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let store = WatermarkStore::default();
        // Empty store should not write a file
        let result = store.persist(dir.path());
        assert!(result.is_ok());
        assert!(!dir.path().join("watermark.json").exists());
    }

    #[test]
    fn test_watermark_persist_overwrites_previous() {
        let dir = tempfile::tempdir().unwrap();

        // First write
        let mut store = WatermarkStore::default();
        store.marks.insert(
            "owner/repo".to_string(),
            Watermark {
                last_delivery_id: Some("first".to_string()),
                last_event_id: None,
                last_processed_at: Utc::now(),
            },
        );
        store.persist(dir.path()).unwrap();

        // Second write with different data
        store.marks.insert(
            "owner/repo".to_string(),
            Watermark {
                last_delivery_id: Some("second".to_string()),
                last_event_id: None,
                last_processed_at: Utc::now(),
            },
        );
        store.persist(dir.path()).unwrap();

        // Load and verify second write replaced first
        let loaded = load_watermarks(dir.path());
        assert_eq!(
            loaded.marks.get("owner/repo").unwrap().last_delivery_id,
            Some("second".to_string())
        );
    }

    #[test]
    fn test_new_watermark_store_creates_empty() {
        let store = new_watermark_store();
        let locked = store.blocking_read();
        assert!(locked.marks.is_empty());
    }
}
