//! Catch-up orchestration: replay missed webhook events on server startup.
//!
//! When Yoke starts up (or restarts after downtime), it may have missed webhook
//! events that occurred while it was offline. This module queries the platform
//! delivery/event APIs for events newer than the persisted watermark, converts
//! them into [`DispatchMessage`]s, and sends them through the same dispatcher
//! channel used by live webhooks. The existing dedup sets naturally prevent
//! re-processing of events that were already handled.
//!
//! # First-run behavior
//!
//! On the very first run (no persisted watermarks for any repo), catch-up is
//! **skipped entirely** before entering the per-repo loop because there is no
//! baseline timestamp to compare against. Without watermarks, replaying all
//! events within the `catch_up_max_age_hours` window could trigger unexpected
//! behaviour. Catch-up resumes on subsequent starts once watermarks have been
//! established by processing live webhook events.
//!
//! If some repos have watermarks and others don't (e.g. a new repo was added
//! to config after a previous run), the per-repo check still skips individual
//! repos with no watermark.
//!
//! # Flow
//!
//! ```text
//! Server startup
//!   ├── Load watermarks from watermark.json
//!   ├── Watermark store empty? → Skip catch-up entirely (first run)
//!   ├── For each configured repo:
//!   │     ├── No watermark for this repo? → Skip catch-up for this repo
//!   │     ├── Get last_processed_at from watermark
//!   │     ├── Get hook_id for our webhook (from list_webhooks)
//!   │     ├── GitHub: list_deliveries(owner, repo, hook_id)
//!   │     │   └── Filter deliveries where delivered_at > last_processed_at
//!   │     ├── GitLab: list_project_events(project_id, after=last_processed_at)
//!   │     │   └── Filter events where created_at > last_processed_at
//!   │     └── For each missed event:
//!   │           ├── Convert to DispatchMessage (same format as live webhooks)
//!   │           └── Send to dispatcher channel (reuse existing dispatch path)
//!   └── Log summary: "Catch-up complete: {n} events replayed across {m} repos"
//! ```

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;
use tracing::instrument;

use crate::config::{Config, Platform, Repo, ServerConfig};
use crate::dispatcher::{DispatchMessage, WatermarkStore};
use crate::webhook::TriggerEvent;
use crate::webhook::github_api::GitHubClient;
use crate::webhook::gitlab_api::GitLabClient;

/// Summary of a catch-up run, used for logging.
#[derive(Debug, Default)]
pub struct CatchUpSummary {
    /// Total number of events replayed across all repos.
    pub events_replayed: usize,
    /// Total number of events skipped (already in dedup, no matching trigger, etc.).
    pub events_skipped: usize,
    /// Number of repos processed.
    pub repos_processed: usize,
    /// Number of repos that had errors during catch-up.
    pub repos_with_errors: usize,
}

impl std::fmt::Display for CatchUpSummary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Catch-up complete: {} events replayed, {} skipped across {} repos ({} errors)",
            self.events_replayed, self.events_skipped, self.repos_processed, self.repos_with_errors
        )
    }
}

/// Run catch-up for all configured repositories.
///
/// For each repo in `config.repos`, queries the platform API for events newer
/// than the persisted watermark, converts them into `DispatchMessage`s, and
/// sends them through the dispatcher channel. The `catch_up_max_age_hours`
/// from `server_config` limits how far back to look.
///
/// Returns a summary of the catch-up run for logging.
///
/// If `catch_up_enabled` is `false` in the server config, returns immediately
/// with an empty summary.
#[instrument(skip_all)]
pub async fn run_catch_up(
    config: &Config,
    server_config: &ServerConfig,
    watermark_store: &Arc<RwLock<WatermarkStore>>,
    dispatch_tx: &tokio::sync::mpsc::Sender<DispatchMessage>,
) -> CatchUpSummary {
    if !server_config.catch_up_enabled {
        tracing::info!("Catch-up disabled, skipping");
        return CatchUpSummary::default();
    }

    if config.repos.is_empty() {
        tracing::info!("No repos configured, skipping catch-up");
        return CatchUpSummary::default();
    }

    // On the very first run (no watermarks at all), skip catch-up entirely.
    // Without a persisted baseline timestamp, replaying events within the
    // max_age window could trigger unexpected behaviour. Catch-up will
    // resume on subsequent starts once watermarks have been established
    // by processing live webhook events.
    {
        let store = watermark_store.read().await;
        if store.marks.is_empty() {
            tracing::info!(
                repo_count = config.repos.len(),
                "No watermarks found — skipping catch-up on first run"
            );
            return CatchUpSummary::default();
        }
    }

    // Derive the cutoff time from max_age_hours
    let max_age = Duration::from_secs(server_config.catch_up_max_age_hours * 3600);
    let cutoff = Utc::now()
        - chrono::Duration::from_std(max_age).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "Invalid max_age, defaulting to 24h");
            chrono::Duration::hours(24)
        });

    tracing::info!(
        max_age_hours = server_config.catch_up_max_age_hours,
        cutoff = %cutoff,
        repo_count = config.repos.len(),
        "Starting catch-up"
    );

    let mut summary = CatchUpSummary::default();

    for repo in &config.repos {
        match run_catch_up_for_repo(config, repo, watermark_store, dispatch_tx, cutoff).await {
            Ok(repo_summary) => {
                summary.events_replayed += repo_summary.events_replayed;
                summary.events_skipped += repo_summary.events_skipped;
                summary.repos_processed += 1;
                if repo_summary.had_error {
                    summary.repos_with_errors += 1;
                }
            }
            Err(e) => {
                tracing::error!(
                    owner = %repo.owner,
                    repo = %repo.repo,
                    error = %e,
                    "Catch-up failed for repo"
                );
                summary.repos_processed += 1;
                summary.repos_with_errors += 1;
            }
        }
    }

    tracing::info!(%summary);
    summary
}

/// Per-repo catch-up result.
struct RepoCatchUpResult {
    events_replayed: usize,
    events_skipped: usize,
    had_error: bool,
}

/// Run catch-up for a single repository.
///
/// If no watermark exists for this repo, catch-up is skipped entirely —
/// on the very first run there is no baseline to compare against, so
/// replaying all events within the `catch_up_max_age_hours` window could
/// trigger unexpected behaviour.
///
/// When a watermark exists, queries the platform API for events newer
/// than the watermark (or the cutoff time, whichever is more recent),
/// and sends them through the dispatcher channel.
async fn run_catch_up_for_repo(
    config: &Config,
    repo: &Repo,
    watermark_store: &Arc<RwLock<WatermarkStore>>,
    dispatch_tx: &tokio::sync::mpsc::Sender<DispatchMessage>,
    cutoff: DateTime<Utc>,
) -> Result<RepoCatchUpResult, String> {
    let repo_key = format!("{}/{}", repo.owner, repo.repo);

    // Determine the starting point from the watermark.
    // If no watermark exists for this repo, skip catch-up entirely —
    // on the very first run we have no baseline to compare against, so
    // replaying all events within the max_age window could trigger
    // unexpected behaviour.
    let since = {
        let store = watermark_store.read().await;
        match store.marks.get(&repo_key) {
            Some(wm) => {
                // Use the more recent of watermark and cutoff
                if wm.last_processed_at > cutoff {
                    wm.last_processed_at
                } else {
                    cutoff
                }
            }
            None => {
                tracing::info!(
                    owner = %repo.owner,
                    repo = %repo.repo,
                    "No watermark found for repo, skipping catch-up on first run"
                );
                return Ok(RepoCatchUpResult {
                    events_replayed: 0,
                    events_skipped: 0,
                    had_error: false,
                });
            }
        }
    };

    tracing::info!(
        owner = %repo.owner,
        repo = %repo.repo,
        since = %since,
        "Running catch-up for repo"
    );

    match config.platform {
        Platform::Github => catch_up_github(config, repo, since, dispatch_tx).await,
        Platform::Gitlab => catch_up_gitlab(config, repo, since, dispatch_tx).await,
    }
}

/// Run catch-up for a GitHub repository.
///
/// 1. Find the hook_id for our webhook URL via `list_webhooks`.
/// 2. List deliveries for that hook, filtering by `delivered_at > since`.
/// 3. For each missed delivery, fetch the full delivery detail and parse
///    it through `dispatch_webhook` (reusing the live webhook parsing path).
/// 4. Send the resulting `DispatchMessage` to the dispatcher.
async fn catch_up_github(
    config: &Config,
    repo: &Repo,
    since: DateTime<Utc>,
    dispatch_tx: &tokio::sync::mpsc::Sender<DispatchMessage>,
) -> Result<RepoCatchUpResult, String> {
    let token = std::env::var(crate::config::env::GITHUB_TOKEN)
        .map_err(|_| "GITHUB_TOKEN not set".to_string())?;

    // Use the default GitHub API URL. GitHub Enterprise support would
    // require adding a `github_url` field to GithubConfig.
    let client = GitHubClient::new(token, None);

    // Step 1: Find our webhook's hook_id
    let webhooks = client
        .list_webhooks(&repo.owner, &repo.repo)
        .await
        .map_err(|e| format!("Failed to list webhooks: {e}"))?;

    let webhook_host = &config.server.webhook_host;
    let webhook_url = format!("https://{webhook_host}/webhook");

    let our_hook = webhooks.iter().find(|w| w.payload_url() == webhook_url);

    let hook_id = match our_hook {
        Some(hook) => hook.id,
        None => {
            tracing::warn!(
                owner = %repo.owner,
                repo = %repo.repo,
                url = %webhook_url,
                "No webhook found matching our URL, skipping catch-up"
            );
            return Ok(RepoCatchUpResult {
                events_replayed: 0,
                events_skipped: 0,
                had_error: false,
            });
        }
    };

    // Step 2: List deliveries for this hook, filtering by time
    let deliveries = client
        .list_deliveries(&repo.owner, &repo.repo, hook_id)
        .await
        .map_err(|e| format!("Failed to list deliveries: {e}"))?;

    let missed: Vec<_> = deliveries
        .iter()
        .filter(|d| d.delivered_at > since)
        .collect();

    if missed.is_empty() {
        tracing::info!(
            owner = %repo.owner,
            repo = %repo.repo,
            "No missed deliveries found"
        );
        return Ok(RepoCatchUpResult {
            events_replayed: 0,
            events_skipped: 0,
            had_error: false,
        });
    }

    tracing::info!(
        owner = %repo.owner,
        repo = %repo.repo,
        missed_count = missed.len(),
        "Found missed deliveries"
    );

    // Step 3: Fetch full delivery details and replay
    let mut events_replayed = 0;
    let mut events_skipped = 0;

    for delivery in &missed {
        match client
            .get_delivery(&repo.owner, &repo.repo, hook_id, delivery.id)
            .await
        {
            Ok(detail) => {
                // The payload from GitHub's delivery API is a JSON object.
                // Serialize it to a string for parsing through the webhook
                // dispatch path (which expects a raw JSON string, same as a
                // live webhook body).
                let body = match serde_json::to_string(&detail.request.payload) {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::warn!(
                            owner = %repo.owner,
                            repo = %repo.repo,
                            delivery_id = delivery.id,
                            error = %e,
                            "Failed to serialize delivery payload, skipping"
                        );
                        events_skipped += 1;
                        continue;
                    }
                };
                // Parse the delivery body through the webhook dispatch path,
                // skipping signature verification (we trust the API response).
                match replay_github_delivery(&detail.event, &body, &detail.guid) {
                    Some(trigger_event) => {
                        // Before dispatching, check whether the source entity
                        // (issue or PR) is still in an actionable state.
                        // Closed issues and merged/closed PRs are skipped.
                        if let Some(reason) =
                            check_entity_stale(&client, &repo.owner, &repo.repo, &trigger_event)
                                .await
                        {
                            tracing::info!(
                                owner = %repo.owner,
                                repo = %repo.repo,
                                event_id = %trigger_event.event_id,
                                reason = %reason,
                                "Skipping catch-up event: entity no longer actionable"
                            );
                            events_skipped += 1;
                            continue;
                        }

                        let msg = DispatchMessage {
                            event: trigger_event,
                        };
                        if dispatch_tx.send(msg).await.is_err() {
                            tracing::error!(
                                owner = %repo.owner,
                                repo = %repo.repo,
                                delivery_id = %delivery.guid,
                                "Failed to send catch-up event to dispatcher (channel closed)"
                            );
                        } else {
                            events_replayed += 1;
                        }
                    }
                    None => {
                        events_skipped += 1;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    owner = %repo.owner,
                    repo = %repo.repo,
                    delivery_id = delivery.id,
                    error = %e,
                    "Failed to fetch delivery detail, skipping"
                );
                events_skipped += 1;
            }
        }

        // Rate limiting: small delay between API calls to avoid hitting
        // GitHub's secondary rate limits during catch-up.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    tracing::info!(
        owner = %repo.owner,
        repo = %repo.repo,
        events_replayed,
        events_skipped,
        "Catch-up complete for repo"
    );

    Ok(RepoCatchUpResult {
        events_replayed,
        events_skipped,
        had_error: false,
    })
}

/// Check whether the source entity for a catch-up event is still actionable.
///
/// Queries the GitHub API for the current state of the issue or PR referenced
/// by the trigger event.  Returns `Some(reason)` if the entity should be
/// skipped (closed issue, merged/closed PR), or `None` if the entity is
/// still open and the event should be replayed.
///
/// On API errors, returns `None` (don't block catch-up on transient failures).
async fn check_entity_stale(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    event: &TriggerEvent,
) -> Option<String> {
    use crate::workflow::TriggerType::*;

    match &event.trigger_type {
        GithubIssueAssigned { .. } => {
            // Issue assignment: check if the issue is closed.
            let issue_number = event.variables.get("issue_number")?;
            let number: u64 = issue_number.parse().ok()?;

            match client.get_issue(owner, repo, number).await {
                Ok(issue) => {
                    if issue.state == "closed" {
                        Some(format!(
                            "issue #{} is closed{}",
                            number,
                            issue
                                .state_reason
                                .as_ref()
                                .map(|r| format!(" ({r})"))
                                .unwrap_or_default()
                        ))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    // Don't block catch-up on API errors — log and proceed.
                    tracing::warn!(
                        owner,
                        repo,
                        issue_number = number,
                        error = %e,
                        "Failed to check issue state, proceeding with replay"
                    );
                    None
                }
            }
        }
        GithubIssueCommentMention { .. } => {
            // Issue comment events may reference either an issue or a PR
            // (GitHub sends issue_comment for PR comments too).  Check which
            // key is present to determine the entity type.
            if let Some(pr_number) = event.variables.get("pr_number") {
                let number: u64 = pr_number.parse().ok()?;
                check_pr_stale(client, owner, repo, number).await
            } else if let Some(issue_number) = event.variables.get("issue_number") {
                let number: u64 = issue_number.parse().ok()?;
                check_issue_stale(client, owner, repo, number).await
            } else {
                None
            }
        }
        GithubPullRequestReview | GithubPullRequestCommentMention { .. } => {
            // PR-related triggers: check if the PR is closed or merged.
            let pr_number = event.variables.get("pr_number")?;
            let number: u64 = pr_number.parse().ok()?;
            check_pr_stale(client, owner, repo, number).await
        }
        // GitLab triggers are handled in the GitLab catch-up path.
        _ => None,
    }
}

/// Check whether a GitHub issue is closed (no longer actionable).
async fn check_issue_stale(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Option<String> {
    match client.get_issue(owner, repo, number).await {
        Ok(issue) => {
            if issue.state == "closed" {
                Some(format!(
                    "issue #{} is closed{}",
                    number,
                    issue
                        .state_reason
                        .as_ref()
                        .map(|r| format!(" ({r})"))
                        .unwrap_or_default()
                ))
            } else {
                None
            }
        }
        Err(e) => {
            tracing::warn!(
                owner,
                repo,
                issue_number = number,
                error = %e,
                "Failed to check issue state, proceeding with replay"
            );
            None
        }
    }
}

/// Check whether a GitHub PR is merged or closed (no longer actionable).
async fn check_pr_stale(
    client: &GitHubClient,
    owner: &str,
    repo: &str,
    number: u64,
) -> Option<String> {
    match client.get_pull_request(owner, repo, number).await {
        Ok(pr) => {
            if pr.merged_at.is_some() {
                Some(format!("PR #{} is merged", number))
            } else if pr.state == "closed" {
                Some(format!("PR #{} is closed (not merged)", number))
            } else {
                None
            }
        }
        Err(e) => {
            tracing::warn!(
                owner,
                repo,
                pr_number = number,
                error = %e,
                "Failed to check PR state, proceeding with replay"
            );
            None
        }
    }
}

/// Check whether the source entity for a GitLab catch-up event is still
/// actionable.
///
/// Returns `Some(reason)` if the entity should be skipped (closed issue,
/// merged/closed MR), or `None` if the entity is still open.
/// On API errors, returns `None` (don't block catch-up on transient failures).
async fn check_gitlab_entity_stale(
    client: &crate::webhook::gitlab_api::GitLabClient,
    project_id: &str,
    event: &crate::webhook::gitlab_api::ProjectEvent,
    trigger_event: &TriggerEvent,
) -> Option<String> {
    use crate::workflow::TriggerType::*;

    match &trigger_event.trigger_type {
        GitlabIssueAssigned { .. } | GitlabIssueMention { .. } => {
            let iid = event.target_id?;
            match client.get_issue(project_id, iid).await {
                Ok(issue) => {
                    if issue.state == "closed" {
                        Some(format!("GitLab issue !{} is closed", iid))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        project_id,
                        iid,
                        error = %e,
                        "Failed to check GitLab issue state, proceeding with replay"
                    );
                    None
                }
            }
        }
        GitlabMergeRequestReview | GitlabMergeRequestCommentMention { .. } => {
            let iid = event.target_id?;
            match client.get_merge_request(project_id, iid).await {
                Ok(mr) => {
                    if mr.state == "merged" {
                        Some(format!("GitLab MR !{} is merged", iid))
                    } else if mr.state == "closed" {
                        Some(format!("GitLab MR !{} is closed", iid))
                    } else {
                        None
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        project_id,
                        iid,
                        error = %e,
                        "Failed to check GitLab MR state, proceeding with replay"
                    );
                    None
                }
            }
        }
        // Unknown or push-based triggers — no entity to check.
        _ => None,
    }
}

/// Parse a GitHub delivery body into a TriggerEvent for replay.
///
/// This reuses the same parsing logic as the live webhook handler
/// (`parse_github_event` + `map_to_trigger_event`), but skips HMAC
/// verification since the delivery was fetched directly from GitHub's API.
fn replay_github_delivery(
    event_header: &str,
    body: &str,
    delivery_guid: &str,
) -> Option<TriggerEvent> {
    use crate::webhook::github::{map_to_trigger_event, parse_github_event};

    let event = parse_github_event(event_header, body.as_bytes()).ok()?;
    let trigger_type = map_to_trigger_event(&event)?;

    let repo_path = event.repository.full_name.clone();
    let event_id = match &event.payload {
        crate::webhook::github::GitHubPayload::Issues(p) => {
            format!("issue-{}", p.issue.number)
        }
        crate::webhook::github::GitHubPayload::IssueComment(p) => {
            if p.issue.pull_request.is_some() {
                format!("pr-{}-comment-{}", p.issue.number, p.comment.id)
            } else {
                format!("issue-{}-comment-{}", p.issue.number, p.comment.id)
            }
        }
        crate::webhook::github::GitHubPayload::PullRequestReview(p) => {
            format!("pr-{}-review-{}", p.pull_request.number, p.review.id)
        }
        crate::webhook::github::GitHubPayload::PullRequestReviewComment(p) => {
            format!("pr-{}-comment-{}", p.pull_request.number, p.comment.id)
        }
    };

    // Extract trigger-specific variables (same logic as handle_github_webhook)
    let mut variables = std::collections::HashMap::new();
    match &event.payload {
        crate::webhook::github::GitHubPayload::Issues(p) => {
            variables.insert("issue_number".to_string(), p.issue.number.to_string());
            variables.insert(
                "assignee".to_string(),
                p.issue
                    .assignee
                    .as_ref()
                    .map(|a| a.login.clone())
                    .unwrap_or_default(),
            );
            variables.insert("issue_title".to_string(), p.issue.title.clone());
            variables.insert(
                "issue_body".to_string(),
                p.issue.body.clone().unwrap_or_default(),
            );
        }
        crate::webhook::github::GitHubPayload::IssueComment(p) => {
            if p.issue.pull_request.is_some() {
                variables.insert("pr_number".to_string(), p.issue.number.to_string());
            } else {
                variables.insert("issue_number".to_string(), p.issue.number.to_string());
            }
            variables.insert("comment_id".to_string(), p.comment.id.to_string());
            variables.insert(
                "comment_body".to_string(),
                p.comment.body.clone().unwrap_or_default(),
            );
        }
        crate::webhook::github::GitHubPayload::PullRequestReview(p) => {
            variables.insert("pr_number".to_string(), p.pull_request.number.to_string());
            variables.insert("review_id".to_string(), p.review.id.to_string());
            variables.insert(
                "review_body".to_string(),
                p.review.body.clone().unwrap_or_default(),
            );
        }
        crate::webhook::github::GitHubPayload::PullRequestReviewComment(p) => {
            variables.insert("pr_number".to_string(), p.pull_request.number.to_string());
            variables.insert(
                "review_id".to_string(),
                p.comment
                    .pull_request_review_id
                    .unwrap_or(p.comment.id)
                    .to_string(),
            );
            variables.insert("comment_id".to_string(), p.comment.id.to_string());
            variables.insert(
                "comment_body".to_string(),
                p.comment.body.clone().unwrap_or_default(),
            );
        }
    }

    // Extract the actor
    let actor = match &event.payload {
        crate::webhook::github::GitHubPayload::Issues(p) => p.sender.login.clone(),
        crate::webhook::github::GitHubPayload::IssueComment(p) => p.sender.login.clone(),
        crate::webhook::github::GitHubPayload::PullRequestReview(p) => p
            .review
            .user
            .as_ref()
            .map(|u| u.login.clone())
            .unwrap_or_else(|| p.sender.login.clone()),
        crate::webhook::github::GitHubPayload::PullRequestReviewComment(p) => {
            p.sender.login.clone()
        }
    };

    Some(TriggerEvent {
        trigger_type,
        repo_path,
        event_id,
        actor,
        variables,
        delivery_id: Some(delivery_guid.to_string()),
        branch: None,
    })
}

/// Run catch-up for a GitLab repository.
///
/// 1. Call `list_project_events` with the `since` timestamp.
/// 2. Filter events by `created_at > since`.
/// 3. Convert each event to a `TriggerEvent` using
///    [`ProjectEvent::try_into_trigger_event`].
/// 4. Send the resulting `DispatchMessage` to the dispatcher.
async fn catch_up_gitlab(
    config: &Config,
    repo: &Repo,
    since: DateTime<Utc>,
    dispatch_tx: &tokio::sync::mpsc::Sender<DispatchMessage>,
) -> Result<RepoCatchUpResult, String> {
    let token = std::env::var(crate::config::env::GITLAB_TOKEN)
        .map_err(|_| "GITLAB_TOKEN not set".to_string())?;

    // Determine the GitLab API base URL
    let base_url = config.gitlab_url.as_ref().map(|u| {
        let s = u.to_string();
        format!("{}/api/v4", s.trim_end_matches('/'))
    });

    // Also check config.gitlab.gitlab_url as fallback
    let base_url = base_url.or_else(|| {
        config.gitlab.as_ref().map(|g| {
            let s = g.gitlab_url.to_string();
            format!("{}/api/v4", s.trim_end_matches('/'))
        })
    });

    let client = GitLabClient::new(token, base_url);

    // Use owner/repo as project_id (URL-encoded)
    let project_id = format!("{}%2F{}", repo.owner, repo.repo);
    let since_iso = since.to_rfc3339();

    let events = client
        .list_project_events(&project_id, &since_iso)
        .await
        .map_err(|e| format!("Failed to list project events: {e}"))?;

    // Filter events newer than `since` (the API `after` param is inclusive,
    // so we filter client-side to get strictly newer events)
    let missed: Vec<_> = events
        .into_iter()
        .filter(|e| e.created_at > since)
        .collect();

    if missed.is_empty() {
        tracing::info!(
            owner = %repo.owner,
            repo = %repo.repo,
            "No missed events found"
        );
        return Ok(RepoCatchUpResult {
            events_replayed: 0,
            events_skipped: 0,
            had_error: false,
        });
    }

    tracing::info!(
        owner = %repo.owner,
        repo = %repo.repo,
        missed_count = missed.len(),
        "Found missed events"
    );

    let mut events_replayed = 0;
    let mut events_skipped = 0;
    let repo_path = format!("{}/{}", repo.owner, repo.repo);

    for event in missed {
        match event.clone().try_into_trigger_event(repo_path.clone()) {
            Some(trigger_event) => {
                // Before dispatching, check whether the source entity
                // (issue or MR) is still in an actionable state.
                if let Some(reason) =
                    check_gitlab_entity_stale(&client, &project_id, &event, &trigger_event).await
                {
                    tracing::info!(
                        owner = %repo.owner,
                        repo = %repo.repo,
                        event_id = %trigger_event.event_id,
                        reason = %reason,
                        "Skipping catch-up event: entity no longer actionable"
                    );
                    events_skipped += 1;
                    continue;
                }

                let msg = DispatchMessage {
                    event: trigger_event,
                };
                if dispatch_tx.send(msg).await.is_err() {
                    tracing::error!(
                        owner = %repo.owner,
                        repo = %repo.repo,
                        event_id = event.id,
                        "Failed to send catch-up event to dispatcher (channel closed)"
                    );
                } else {
                    events_replayed += 1;
                }
            }
            None => {
                events_skipped += 1;
            }
        }

        // Rate limiting: small delay between events during catch-up
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    tracing::info!(
        owner = %repo.owner,
        repo = %repo.repo,
        events_replayed,
        events_skipped,
        "Catch-up complete for repo"
    );

    Ok(RepoCatchUpResult {
        events_replayed,
        events_skipped,
        had_error: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AgentConfig, Config, Platform, Repo, ServerConfig};
    use crate::dispatcher::Watermark;

    fn test_config() -> Config {
        Config {
            platform: Platform::Github,
            repos: vec![Repo {
                owner: "test-owner".to_string(),
                repo: "test-repo".to_string(),
            }],
            agents: vec![AgentConfig {
                name: "test".to_string(),
                base_url: "http://localhost:8000".parse().unwrap(),
            }],
            runtime: Default::default(),
            server: ServerConfig {
                host: "0.0.0.0".to_string(),
                webhook_host: "yoke.example.com".to_string(),
                port: 8644,
                max_body_size: 1_048_576,
                catch_up_enabled: true,
                catch_up_max_age_hours: 24,
            },
            github: None,
            gitlab: None,
            gitlab_url: None,
        }
    }

    #[tokio::test]
    async fn test_catch_up_disabled() {
        let mut config = test_config();
        config.server.catch_up_enabled = false;
        let watermark_store = Arc::new(RwLock::new(WatermarkStore::default()));
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        let summary = run_catch_up(&config, &config.server, &watermark_store, &tx).await;
        assert_eq!(summary.events_replayed, 0);
        assert_eq!(summary.repos_processed, 0);
    }

    #[tokio::test]
    async fn test_catch_up_no_repos() {
        let mut config = test_config();
        config.repos = vec![];
        let watermark_store = Arc::new(RwLock::new(WatermarkStore::default()));
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        let summary = run_catch_up(&config, &config.server, &watermark_store, &tx).await;
        assert_eq!(summary.events_replayed, 0);
        assert_eq!(summary.repos_processed, 0);
    }

    #[tokio::test]
    async fn test_catch_up_watermark_determines_since() {
        // When a watermark exists and is newer than the cutoff, it should be used
        let config = test_config();
        let mut marks = std::collections::HashMap::new();
        let recent_time = Utc::now() - chrono::Duration::hours(2);
        marks.insert(
            "test-owner/test-repo".to_string(),
            Watermark {
                last_delivery_id: Some("abc-123".to_string()),
                last_event_id: Some("event-1".to_string()),
                last_processed_at: recent_time,
            },
        );
        let watermark_store = Arc::new(RwLock::new(WatermarkStore { marks }));
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        // This will try to do actual API calls and fail (no GITHUB_TOKEN set),
        // but we just need to verify the flow reaches the per-repo handler.
        // The function will return an error because there's no GITHUB_TOKEN.
        let summary = run_catch_up(&config, &config.server, &watermark_store, &tx).await;
        // The repo should be processed (even if it errors)
        assert_eq!(summary.repos_processed, 1);
        assert_eq!(summary.repos_with_errors, 1);
    }

    #[tokio::test]
    async fn test_catch_up_skips_on_first_run_no_watermark() {
        // When no watermarks exist at all, catch-up should be skipped entirely
        // at the top level to avoid iterating through every repo and to
        // prevent replaying all events within the max_age window.
        let config = test_config();
        let watermark_store = Arc::new(RwLock::new(WatermarkStore::default()));
        let (tx, _rx) = tokio::sync::mpsc::channel(100);

        let summary = run_catch_up(&config, &config.server, &watermark_store, &tx).await;
        // No repos should be processed — the top-level check skips everything
        assert_eq!(summary.repos_processed, 0);
        assert_eq!(summary.repos_with_errors, 0);
        assert_eq!(summary.events_replayed, 0);
        assert_eq!(summary.events_skipped, 0);
    }

    #[test]
    fn test_replay_github_delivery_issues_assigned() {
        let body = r#"{
            "action": "assigned",
            "issue": {
                "number": 42,
                "title": "Test issue",
                "body": "Test body",
                "assignee": { "login": "assignee-user" },
                "assignees": [{ "login": "assignee-user" }]
            },
            "sender": { "login": "sender-user" },
            "repository": { "full_name": "owner/repo" }
        }"#;

        let result = replay_github_delivery("issues", body, "delivery-guid-123");
        assert!(result.is_some());
        let event = result.unwrap();
        assert_eq!(event.repo_path, "owner/repo");
        assert_eq!(event.event_id, "issue-42");
        assert_eq!(event.actor, "sender-user");
        assert_eq!(event.delivery_id, Some("delivery-guid-123".to_string()));
    }

    #[test]
    fn test_replay_github_delivery_no_matching_trigger() {
        // An "opened" action on issues doesn't map to any trigger type
        let body = r#"{
            "action": "opened",
            "issue": {
                "number": 42,
                "title": "Test issue",
                "body": "Test body"
            },
            "sender": { "login": "sender-user" },
            "repository": { "full_name": "owner/repo" }
        }"#;

        let result = replay_github_delivery("issues", body, "delivery-guid-456");
        assert!(result.is_none());
    }

    #[test]
    fn test_replay_github_delivery_unknown_event_type() {
        let body = r#"{"action": "created"}"#;
        let result = replay_github_delivery("unknown_event", body, "delivery-guid-789");
        assert!(result.is_none());
    }

    #[test]
    fn test_catch_up_summary_display() {
        let summary = CatchUpSummary {
            events_replayed: 5,
            events_skipped: 2,
            repos_processed: 3,
            repos_with_errors: 1,
        };
        let display = format!("{summary}");
        assert!(display.contains("5 events replayed"));
        assert!(display.contains("2 skipped"));
        assert!(display.contains("3 repos"));
        assert!(display.contains("1 errors"));
    }
}
