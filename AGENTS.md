# Yoke — Agent Notes

## Project Structure

```
src/
  lib.rs       — Library root (pub mod re-exports for integration tests; includes webhooks module)
  main.rs      — CLI entrypoint (initializes tracing subscriber, loads config, loads workflows, validates agents & triggers, starts server; handles `webhooks` subcommand)
  cli.rs       — CLI argument parsing (clap derive) with `webhooks` subcommand support
  config.rs    — Configuration parsing, validation, error types, and env var name constants (env module)
  dispatcher.rs — Concurrency control (Dispatcher + Semaphore), deduplication (DedupSets, SharedDedupSets), persistence, and workspace directory management
  harness.rs   — Hermes API client harness (HermesClient, request/response types, StepResult, error handling)
  file_log.rs  — Workflow step audit files: write `.prompt` and `.log` files per agent step (called from runner.rs)
  reload.rs    — File watcher for workflow hot-reload (notify crate, debouncing, ReloadMessage types, WorkflowState, reload_workflows)
  server.rs    — axum HTTP server with health, readiness, webhook endpoint, and HTTP header constants (headers module)
  workflow.rs  — Workflow TOML parsing, validation (including template variable validation), error types, and trigger type string constants (triggers module)
  git.rs        — Git repository operations (clone, pull, worktree, auth callbacks, dirty-check)
  github_api.rs — GitHub REST API client (GitHubClient, Webhook, WebhookConfig, WebhookOrchestrationSummary, GitHubError with ValidationError variant, list/create/update/delete webhooks, find_webhook_by_url, ensure_webhook, orchestrate_webhooks, map_status_with_body for response body capture in errors)
  webhooks.rs  — Unified webhook management (WebhookClient enum, GitHubWebhookClient, GitLabWebhookClient, WebhookInfo, WebhookConfig, WebhookError, AddSummary, RemoveSummary, webhooks_list/webhooks_remove/webhooks_add handlers) — CLI subcommand handler for `yoke webhooks`
  template.rs  — Template rendering with `{{variable}}` substitution, validation, and variable extraction
  hooks.rs     — Hook definitions (FileNotEmpty, FileContains) and run_hook dispatcher (paths and text are rendered by the runner before execution)
  runner.rs    — Workflow runner: sequential step execution with template vars, hook path/text rendering, and fail-fast
  webhook/     — Webhook handling modules
    mod.rs       — Shared types (TriggerEvent, WebhookError) and dispatch to platform handler
    github.rs   — GitHub webhook: HMAC-SHA256 verification, event parsing, trigger mapping, and GitHub event type string constants
    gitlab.rs   — GitLab webhook: token verification, payload parsing, event mapping, and GitLab event type string constants
    gitlab_api.rs — GitLab REST API client (GitLabClient, GitLabWebhook, WebhookConfig, list/create/update/delete_webhooks, find_webhook_by_url, error types)
tests/
  dispatcher_tests.rs — Integration tests for dispatcher (full dispatch flow, dedup, concurrency, shutdown, persistence)
  git_integration_tests.rs — Integration tests for git module (clone, pull, worktree, dirty-check with local repos)
  harness_tests.rs   — Integration tests for harness (serialization, response parsing, error file, multi-instance)
  reload_tests.rs    — Integration tests for reload module (file detection, debouncing, .toml filtering)
  reload_integration_tests.rs — Integration tests for hot-reload (add workflow, invalid file preserves state, file removal)
  runner_tests.rs   — Integration tests for workflow runner (step execution, template vars, hooks, fail-fast)
  shutdown_tests.rs  — Integration tests for graceful shutdown (SIGINT/SIGTERM, drain, state persistence)
  webhooks_tests.rs   — Integration tests for webhooks CLI handlers (list, remove, add with mockito HTTP mocking)
```

## Key Design Decisions

- **Canonical `event_id` convention**: Every `TriggerEvent` carries an `event_id` field in canonical form, defined per trigger type in Appendix A of the architecture design. Examples: `issue-42` (GitHub issue assigned or comment mention), `pr-7-review-999` (GitHub PR review), `pr-7-comment-12345` (GitHub PR comment mention), `issue-7` (GitLab issue assigned or mention), `mr-5-review-88` (GitLab MR review), `mr-5-comment-204` (GitLab MR comment mention). Dedup keys, workspace directories, template variables, and log fields all use the canonical form.

- **Library + binary crate**: Yoke is both a library and a binary. `src/lib.rs` re-exports all modules as `pub` so integration tests (`tests/`) can import from `yoke::`. `src/main.rs` uses `use yoke::` imports instead of inline `mod` declarations.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **CLI argument parsing**: Uses `clap` with derive macros. `--config` and `--workflows` have defaults; `--host`, `--webhook-host`, and `--port` override values from `config.toml`. `--webhook-host` sets the external hostname used in webhook registration URLs (useful when binding to `0.0.0.0` but advertising a public hostname).
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes, trigger types, template variables, allowed_users) is done in `Config::validate()` / `Workflow::validate()`.
- **CRITICAL: `allowed_users` is a SECURITY BOUNDARY, not an event-content filter**: `allowed_users` exists to prevent prompt injection attacks by restricting which users can **invoke** a workflow. It must NEVER be conflated with `assigned_to` or `mentioned_user`. Those are event-content filters (they describe who the event is ABOUT). `allowed_users` is an authorization check (it describes who is PERMITTED TO TRIGGER the workflow). The **actor** checked against `allowed_users` is the user who **performed the action** that created the webhook event (extracted from `payload.sender.login` on GitHub, `payload.user.username` on GitLab) — NOT the assignee or mentioned user. For example, when an issue is assigned to alice by bob, the actor is bob (who did the assigning), not alice (who was assigned). `Workflow::validate()` enforces that every workflow has a non-empty `allowed_users` list; omitting it would allow any user to trigger the workflow. The dispatcher extracts the actor from the webhook payload at receipt time and rejects events from unauthorized actors. See the "Trigger Authorization" section in `docs/Architecture Design.md` for the full actor mapping table.
- **Template variable validation**: `Workflow::validate()` checks every step's `prompt_template` for unknown variables and malformed `{{` syntax. `TriggerType::known_variables()` returns the set of valid variables for each trigger variant (global vars like `owner`, `repo`, `output_dir`, `event_id`, `repo_path` plus trigger-specific vars). `extract_variables()` in `src/template.rs` parses `{{…}}` placeholders; unknown or cross-platform variables cause `WorkflowError::Validation` with step name and variable name. This runs at startup via `load_workflows()` and on explicit `validate()` calls, preventing runtime template-render failures.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand, AgentResolution, EnvVar) with `Display` and `Error` impls.
- **Agent resolution**: `resolve_agents(config, workflows)` validates that every `step.agent` in every workflow matches a configured `[[agents]]` name. Returns `ConfigError::AgentResolution` with step name, workflow path, and missing agent.
- **Environment variable validation**: `validate_env_vars(platform)` checks required env vars at startup. `HERMES_API_KEY` is always required; `GITHUB_TOKEN` is required when `platform = "github"`, `GITLAB_TOKEN` when `platform = "gitlab"`. `WEBHOOK_SECRET` is optional as an env var — the webhook secret can be provided via `server.webhook_secret` in `config.toml`. Returns `ConfigError::EnvVar` with a descriptive message.
- **`WEBHOOK_SECRET` env override**: The `WEBHOOK_SECRET` env var is optional and overrides the `config.toml` `server.webhook_secret` value at startup. At least one source (env var or config) must provide a non-empty webhook secret, or the process exits with an error.
- **GitHub API client** (`src/github_api.rs`): `GitHubClient` struct wraps `reqwest::Client` with Bearer token authentication. Public API: `list_webhooks(owner, repo)` with transparent pagination (follows `Link` rel="next" headers), `create_webhook(owner, repo, config)` (POST to `/repos/{owner}/{repo}/hooks`), `update_webhook(owner, repo, webhook_id, config)` (PATCH to `/repos/{owner}/{repo}/hooks/{id}`), `delete_webhook(owner, repo, webhook_id)` (DELETE to `/repos/{owner}/{repo}/hooks/{id}`). `create_webhook` and `update_webhook` validate that `config.events` is non-empty before making the API call, returning `ValidationError` if empty. `WebhookConfig` struct (`url`, `secret`, `events`) is passed to create/update. `find_webhook_by_url(webhooks, url)` searches a webhook list by URL for idempotency checks. `ensure_webhook(owner, repo, config)` is an idempotent orchestrator that lists existing webhooks, finds by URL, and either updates the existing webhook or creates a new one — returning a `WebhookOrchestrationSummary`. `orchestrate_webhooks(repos, config)` iterates over multiple `(owner, repo)` pairs, calling `ensure_webhook` for each and aggregating results into a single summary with `created`, `updated`, and `skipped` counts. Error mapping via `map_status_with_body`: HTTP 401→`Unauthorized`, 404→`NotFound`, 403→`RateLimited`; all other non-success statuses include the response body in the `ApiError` message (format: `"{status} - {body}"`) for easier debugging. `ValidationError` variant is used for client-side validation failures (e.g., empty events list). Unit tests use `mockito` for HTTP mocking and cover: list (success, empty, pagination, 401, 404, 403, 422 with body), create (success 201, empty events validation, 422 with body), update (success 200, empty events validation, not found 404), delete (success 204, not found 404, 422 with body), `find_webhook_by_url` helper, `ensure_webhook` (creates new, updates existing), and `orchestrate_webhooks` (multi-repo with mixed create/update).
- **GitLab API client** (`src/webhook/gitlab_api.rs`): `GitLabClient` struct wraps `reqwest::Client` with Private-Token authentication. Public API: `list_webhooks(project_id)` with transparent pagination (follows `Link` rel="next" headers), `create_webhook(project_id, config)` (POST, returns 201 Created), `update_webhook(project_id, webhook_id, config)` (PUT, returns 200 OK), `delete_webhook(project_id, webhook_id)` (DELETE, returns 204 No Content). `WebhookConfig` struct (`url`, `token`, `push_disabled`, `active`, `events`) is passed to create/update; `Option` fields use `skip_serializing_if` to omit nulls from JSON. Supports self-hosted GitLab via configurable `base_url` (default: `https://gitlab.com/api/v4`). Also provides `find_webhook_by_url` for idempotency checks. Error mapping: HTTP 401→`Unauthorized`, 404→`NotFound`, others→`ApiError`. Unit tests use `mockito` for HTTP mocking and cover: list (success, empty, pagination, 401, 404, 500), create (success 201, 401), update (success 200, 404, 401), delete (success 204, 404, 401), `WebhookConfig` serialization (full, minimal), `find_webhook_by_url` (found, not found, empty), and client construction (default/custom base URL).
- **Webhook management module** (`src/webhooks.rs`): Provides a unified CLI interface for managing webhooks across GitHub and GitLab. `WebhookInfo` is a platform-agnostic struct (`id`, `url`, `secret`, `events`, `active`) that maps from platform-specific types. `WebhookConfig` holds `url`, `secret`, and `events` for creating/updating webhooks. `WebhookClient` is an enum-based dispatcher with `Github(GitHubWebhookClient)` and `Gitlab(GitLabWebhookClient)` variants — this avoids `dyn` dispatch since async trait methods are not dyn-compatible in Rust 2024. `WebhookClient::new(platform, owner, gitlab_url)` reads `GITHUB_TOKEN` or `GITLAB_TOKEN` from the environment (returns `WebhookError::Config` if missing) and constructs the appropriate client. `GitHubWebhookClient::new_with_base_url(token, owner, base_url)` allows constructing a client with a custom API base URL (for testing). `WebhookClient` methods (`list_webhooks`, `create_webhook`, `update_webhook`, `delete_webhook`) delegate to the inner platform client. `WebhookError` enum: `Http` (network errors), `Config` (missing env vars or invalid setup), `Api` (non-success HTTP responses). Handler functions: `webhooks_list(config, client)` lists all webhooks per repo with Yoke URL highlighted; `webhooks_remove(config, client)` removes all webhooks whose URL matches the Yoke server URL (`https://{webhook_host}:{port}/webhook`) across all configured repos, returning a `RemoveSummary` with `deleted`, `not_found`, and `errors` counts; `webhooks_add(config, client, workflows_path)` loads workflow TOML files, derives required events via `Workflow::derive_required_events()`, and idempotently creates or updates webhooks per repo, returning an `AddSummary` with `created`, `updated`, `skipped`, and `errors` counts. `yoke_webhook_url(config)` constructs the webhook URL using `config.server.webhook_host` (the external hostname) rather than `config.server.host` (the bind address), allowing the server to bind locally while advertising a public URL.
- **Trigger platform validation**: After loading config and workflows, `validate_triggers()` checks that each workflow's trigger type prefix matches the configured platform. GitLab triggers (`gitlab_*`) on a GitHub platform (and vice versa) cause a hard exit with a clear error.
- **`TriggerType` enum**: Typed representation of known trigger types (4 GitHub, 4 GitLab). Struct variants carry their required event-content filter fields (`assigned_to`, `mentioned_user`); unit variants (`GithubPullRequestReview`, `GitlabMergeRequestReview`, `GithubLabelAssigned`, `GitlabMergeRequestLabelAssigned`) carry no fields. `allowed_users` is **not** stored on `TriggerType` — it lives on the `Trigger` struct and is checked against `TriggerEvent.actor` by the dispatcher. `TriggerType::from_trigger()` converts a `Trigger` struct; `TriggerType::platform()` returns the owning platform; `TriggerType::label()` returns the string identifier used in workflow TOML files; `TriggerType::webhook_event()` returns the platform-specific webhook event name (e.g., `issues` for GitHub issue triggers, `issues_events` for GitLab issue triggers, `issue_comment` for GitHub comment triggers, `note_events` for GitLab comment/mention triggers, `pull_request_review` / `pull_request_review_comment` for GitHub PR review triggers, `merge_requests_events` for GitLab MR review triggers).
- **`Workflow::derive_required_events()`**: Collects the deduplicated set of platform webhook event names from all triggers defined across a list of workflows. Used by `webhooks add` to determine which events a webhook should subscribe to.
- **`WorkflowError` enum**: Typed errors (Io, Parse, Validation) with `Display` and `Error` impls. Parse/Validation errors include the file path for clear diagnostics.
- **`Workflow.path` field**: Each `Workflow` carries its source file path (populated by `load_workflows`), used for agent resolution error reporting.
- **Hooks module** (`src/hooks.rs`): Defines `Hook` enum with `FileNotEmpty { path }` and `FileContains { path, text }` variants, plus `run_hook()` dispatcher and `HookError` error type. Hooks are configured per-step in TOML using internally-tagged representation (`type = "file_not_empty"` or `type = "file_contains"`). The `Step` struct in `workflow.rs` re-exports `Hook` via `pub use crate::hooks::Hook`. Hook failures return clear error messages: `"File 'X' is empty"`, `"File 'X' not found"`, `"File 'X' does not contain 'Y'"`.
- **Template renderer**: `template::render()` does `{{var}}` substitution, returning `Result<_, TemplateError>` for unknown variables, malformed syntax, and empty templates. `template::extract_variables()` parses `{{…}}` placeholders and returns variable names (for validation), reusing the same parsing logic as `render()`.
- **HTTP server**: `src/server.rs` uses axum with `tower-http` middleware. Three endpoints: `/health` (liveness, returns `{"status":"ok"}`), `/ready` (readiness, returns 200 when dispatcher is running, 503 when shutting down), `/webhook` (POST — dispatches to platform-specific handler based on `platform` config). `CorsLayer::permissive()` allows cross-origin requests. `RequestBodyLimitLayer` enforces `max_body_size` from config. `TraceLayer` provides structured HTTP request logging.
- **Signal handler** (`src/main.rs`): `setup_signal_handler` creates a tokio task that listens for SIGINT and SIGTERM using `tokio::signal::unix`. On the first signal, it sends `true` on a `watch::Sender<bool>` channel which propagates to the HTTP server and dispatcher via the corresponding `watch::Receiver`. On a second signal, it calls `process::exit(1)` for immediate termination. The signal handler also has a 60-second safety timeout — if no second signal arrives, the task exits cleanly after the main runtime has shut down.
- **Graceful shutdown flow**: When the signal handler sends `true` on the watch channel: (1) the axum HTTP server's `with_graceful_shutdown` future detects the change and stops accepting new connections, (2) the dispatcher's `run_with_drain` loop stops consuming from the mpsc channel, (3) the dispatcher waits up to `config.runtime.drain_timeout_secs` (default: 30s) for in-flight workflows to complete (checking `active_count` every 100ms), (4) state is persisted via `Dispatcher::persist_state()` which writes `completed.json` and `failed.json` atomically, (5) the process exits. A second SIGINT/SIGTERM during the drain phase forces an immediate `process::exit(1)`.
- **Configurable drain timeout**: `config.runtime.drain_timeout_secs` (default: 30) controls how long the dispatcher waits for in-flight workflows to complete during shutdown. Set to 0 for instant (non-graceful) shutdown.

- **Webhook dispatch**: The server uses `WebhookHandler` (in `webhook/mod.rs`) which holds the platform config, webhook secret, and an mpsc sender for dispatching verified events. The `AppState` struct contains a `WebhookHandler` instance. The webhook endpoint handler delegates to `WebhookHandler::handle_webhook()`, which authenticates the request, parses the payload, maps it to a `TriggerEvent`, and sends it over the mpsc channel. Returns `Ok(())` on success or a `WebhookError` variant. When the dispatcher channel is closed (receiver dropped), returns `InternalError` → HTTP 503.
- **Git operations module**: `src/git.rs` provides git repository management for the dispatcher pipeline. Public API: `sanitize_branch_name()`, `build_clone_url()`, `GitAuth` (credential callbacks for GitHub/GitLab tokens), `clone_repo()`, `pull_repo()`, `create_worktree()`, `remove_worktree()`, `has_uncommitted_changes()`. Uses the `git2` crate (libgit2 bindings). Token-based auth is wired through `GitAuth` callbacks — GitHub tokens go in the `x-access-token` header, GitLab tokens in the `PRIVATE-TOKEN` header. Branch names are sanitized to `[a-zA-Z0-9._-]` with whitespace collapsed to `-`. Worktrees use sanitized branch names for the administrative directory and support creating from existing branches or branching off HEAD.

- **GitHub webhook handler**: `src/webhook/github.rs` provides HMAC-SHA256 signature verification (`verify_github_signature`), JSON payload parsing (`parse_github_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_github_webhook` function orchestrates the full flow. `GitHubEvent` carries a `repository` field (`RepositoryDetails` with `full_name`) used for `repo_path` extraction. GitHub uses the `issue_comment` webhook event for both issue and PR comments (since PRs are issues in the API). When the `pull_request` field is present in the `issue_comment` payload, the comment is on a PR and maps to `GithubPullRequestCommentMention`; otherwise it maps to `GithubIssueCommentMention`. Trigger-specific variables are extracted per event type: `github_issue_assigned` → `issue_number`, `assignee`, `issue_title`, `issue_body`; `github_issue_comment_mention` → `issue_number`, `comment_id`, `comment_body`; `github_pull_request_review` → `pr_number`, `review_id`, `review_body`; `github_pull_request_comment_mention` → `pr_number`, `review_id`, `comment_id`, `comment_body` (when triggered via `issue_comment` on a PR, `review_id` is not available and only `pr_number`, `comment_id`, and `comment_body` are populated).
- **GitLab webhook handler**: `src/webhook/gitlab.rs` provides constant-time token verification (`verify_gitlab_token`), JSON payload parsing (`parse_gitlab_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_gitlab_webhook` function orchestrates the full flow. `GitLabEvent` has a `repo_path()` method that extracts the project path from the `project` field in the payload, and a `variables()` method that returns trigger-specific template variables: `gitlab_issue_assigned` → `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body`; `gitlab_issue_mention` → `issue_iid`, `note_id`, `comment_body`; `gitlab_merge_request_review` → `mr_iid`, `review_id`, `review_body`; `gitlab_merge_request_comment_mention` → `mr_iid`, `note_id`, `comment_body`.
- **Constant-time comparison**: Both handlers use `subtle::ConstantTimeEq` to prevent timing attacks — GitHub for HMAC signatures, GitLab for token comparison.
- **`WebhookError` enum**: Shared error type (Unauthorized, BadRequest, NoMatchingTrigger, InternalError) in `webhook/mod.rs`, used by `WebhookHandler::handle_webhook()`. `InternalError` is returned when the dispatcher channel is closed, mapping to HTTP 503 Service Unavailable.
- **File watcher / Hot-reload** (`src/reload.rs`): Monitors the `--workflows` directory for `.toml` file changes using the `notify` crate. `setup_file_watcher(workflows_dir, tx)` creates a `notify::RecommendedWatcher`, a bridge thread (sync→async channel adapter), and a debouncing tokio task. The `FileWatcher` handle keeps the watcher alive — dropping it stops the watcher. Debouncing: 500ms after the last event, a single `ReloadMessage` (`FileChanged { path }` or `FileRemoved { path }`) is sent on the async channel. Non-`.toml` files are filtered out in the sync callback before entering the pipeline. The bridge thread converts the synchronous `notify` callback into async events via `blocking_send`. The debounce loop is fully async (`tokio::time::timeout` for the debounce window), avoiding blocking the runtime. `WorkflowState` wraps an `ArcSwap<Vec<(String, Workflow)>>` for lock-free atomic state swaps. `reload_workflows(workflows_dir, config)` performs a full re-load and validation cycle (TOML parsing, agent resolution, trigger platform validation). On success, `WorkflowState::update()` atomically swaps the in-memory workflow set. On validation failure, the error is logged and the previous state is preserved. The reload handler in `main.rs` listens for `ReloadMessage` events and calls `reload_workflows` + `WorkflowState::update` on each notification.
- **`TriggerEvent` struct**: Shared webhook result type in `webhook/mod.rs` with `trigger_type: TriggerType`, `repo_path`, `event_id`, `actor: String`, and `variables: HashMap<String, String>` fields. The `event_id` field holds the canonical event identifier as defined per trigger type in Appendix A of the architecture design — e.g. `issue-42`, `pr-7-review-999`, `issue-42-comment-12345`. This canonical form is constructed by the webhook handlers and must be used as-is throughout the dispatcher (dedup keys, workspace directories, template variables, log fields) without any stripping or transformation. The `actor` field holds the username of the user who performed the action (`sender.login` for GitHub, `user.username` for GitLab) — this is checked against the workflow's `allowed_users` list by the dispatcher. The `variables` map carries trigger-specific variables extracted from the webhook payload (e.g., `issue_number`, `comment_id` for GitHub; `issue_iid`, `note_id` for GitLab). These are merged with global variables (`owner`, `repo`, `output_dir`, `event_id`, `repo_path`) in the dispatcher before template rendering. Sent to the dispatcher via the mpsc channel in `WebhookHandler`.
- **`WebhookHandler` struct**: Holds `platform`, `secret`, and `sender: mpsc::Sender<TriggerEvent>`. Created in `run_server()` with a bounded channel and passed to `AppState`. Derives `Clone`.
- **`AppState` struct**: Contains `webhook_handler: WebhookHandler` and `dispatcher: Dispatcher`. Derives `Clone` for axum state sharing. The `dispatcher` field provides concurrency control (via `tokio::Semaphore`) and deduplication state (`SharedDedupSets`).
- **Dispatcher and concurrency control** (`src/dispatcher.rs`): The `Dispatcher` struct wraps `SharedDedupSets` and an optional `tokio::Semaphore` to coordinate concurrency limiting and deduplication for webhook event processing. `find_matching_workflows` selects workflows whose trigger label matches the event **and** whose `allowed_users` list includes the event's actor — this is the authorization check that enforces the `allowed_users` SECURITY BOUNDARY. When `max_concurrent > 0`, the dispatcher holds a `Semaphore` that caps simultaneous workflow executions; permits are acquired via `acquire_permit()` (returning `Option<OwnedSemaphorePermit>`) or the convenience method `run_with_permit()` which holds the permit for the future's lifetime and releases it on drop (RAII pattern). When `max_concurrent == 0`, the semaphore is `None` and no limiting is applied. An `AtomicUsize` counter (`active_count`) tracks active workflows for observability and shutdown draining. `acquire_permit()` increments `active_count` when a semaphore permit is acquired (`Some` result). `run_with_permit()` decrements it on drop via RAII. `spawn_workflow()` also decrements `active_count` in two code paths: (1) when the spawned task completes (before `drop(permit)`), and (2) on error paths that acquired a permit but won't spawn (e.g., workspace directory creation failure). Both decrements are guarded by `if permit.is_some()` since `max_concurrent == 0` means no permit and no increment. The `Dispatcher` is `Clone` (cheap via `Arc` clones) and is stored in `AppState` for sharing across axum handlers.
- **Dispatcher deduplication** (`src/dispatcher.rs`): Three-set `DedupSets` tracks event lifecycle states (`in_flight`, `completed`, `permanently_failed`). Events are identified by dedup keys formatted as `{owner}/{repo}/{event_id}`, where `event_id` is the canonical form defined per trigger type in the architecture design (Appendix A: Trigger Reference) — e.g. `issue-42` for GitHub issue events, `pr-7-review-999` for PR reviews. The canonical `event_id` from `TriggerEvent.event_id` is used directly without stripping or transformation. `SharedDedupSets` (`Arc<RwLock<DedupSets>>`) provides thread-safe async access. An event is considered a duplicate if its key appears in *any* of the three sets. State transitions: `mark_in_flight` → `mark_completed` (success) or `mark_failed` (permanent failure); `remove_in_flight` allows retry on transient failures.
- **Dedup persistence** (`src/dispatcher.rs`): `FailedEntry` struct records permanently failed events with `{key, timestamp, error}`. `PersistenceError` enum handles IO and JSON errors from file operations. `load_dedup_file` deserializes JSON files (returns `NotFound` for missing, `Json` for corrupted). `save_dedup_file` uses atomic writes — writes to `.json.tmp`, then `rename` to target — to prevent data corruption on crash. `DedupSets::persist_completed` creates the work directory if missing (`create_dir_all`) before saving the `completed` set to `completed.json`. `DedupSets::persist_failed` creates the work directory if missing before appending a `FailedEntry` to `failed.json` (load-append-save pattern; JSON arrays require full rewrite). Error logs include the target file path via a `path` field for diagnostics. `load_persistence` reads `completed.json` and `failed.json` from the work directory at startup, treating missing files as empty sets and logging warnings for corrupted ones. `in_flight` is always empty on load (transient state).
- **Hermes API harness** (`src/harness.rs`): `HermesClient` encapsulates a `reqwest::Client`, `base_url`, and `api_key` for making authenticated POST requests to the Hermes Agent API `/v1/responses` endpoint. `execute_step(instructions, input)` builds a `HermesRequest { instructions, input, store: true }` where `instructions` is `Option<String>` — when `None`, the field is omitted from the JSON payload, sends it via `POST {base_url}/v1/responses` with `Authorization: Bearer *** and parses the response into a `HermesResponse`. Response parsing filters `output` content blocks for `type == "output_text"` and joins their text with newlines. Non-2xx responses write the status code and body to a `.error` file and return `HarnessError::Api`. `HarnessError` has three variants: `Http` (network/request errors), `Api` (non-2xx status with status code and body), and `Io` (file write errors for `.error`). `execute_step_with_error_path` accepts an optional `Path` for the error file (used in tests). `HermesRequest`, `HermesResponse`, and `ContentBlock` derive `Serialize`/`Deserialize` for JSON round-tripping; `ContentBlock` uses `#[serde(rename = "type")]` for the `block_type` field.
- **StepResult struct** (`src/harness.rs`): `StepResult` captures the output of a single agent step execution. Fields: `extracted_message` (the text from `output_text` content blocks), `raw_request` (the full JSON request body sent to the API), and `raw_response` (the full JSON response body received). Both `execute_step` and `execute_step_with_error_path` return `Result<StepResult, HarnessError>` instead of `Result<String, HarnessError>`, enabling audit logging of the full HTTP exchange per step.
- **Workflow step audit files** (`src/file_log.rs`, called from `src/runner.rs`): `write_prompt_file(step_num, step_name, prompt, workspace_dir)` writes a rendered prompt template to `{workspace_dir}/{step_num:02}_{step_name}.prompt` *before* the API call. `write_request_log_file(step_num, step_name, request, workspace_dir)` writes just the request portion to `{workspace_dir}/{step_num:02}_{step_name}.log` *before* the API call, ensuring the request is always logged even if the API call fails. `write_log_file(step_num, step_name, request, response, extracted_message, workspace_dir)` overwrites the log file with the full HTTP exchange *after* a successful API call, with sections for `REQUEST:`, `RESPONSE:`, and `FINAL MESSAGE:`. All three functions create the workspace directory if it doesn't exist. `HermesClient::build_request_body(instructions, input)` serializes the request body for pre-call logging without sending it. File naming uses zero-padded two-digit step numbers (e.g., `00_Plan.log`, `01_Analyze.prompt`). File writing is performed in `WorkflowRunner::execute_step()` using an `AtomicUsize` step counter for zero-based numbering.
- **Workspace directory** (`src/dispatcher.rs`): `workspace_dir(workdir, owner, repo, event_id)` constructs the per-event workspace path `{workdir}/{owner}/{repo}/{event_id}/` per the architecture design doc (Section 11). The dispatcher creates this directory before spawning the workflow task. The actual prompt and log files are written by `WorkflowRunner::execute_step()` using real API request/response data.
- **Workflow runner** (`src/runner.rs`): `WorkflowRunner` orchestrates sequential execution of a `Workflow`. The runner holds `agents: Vec<AgentConfig>`, `api_key: String`, and an `AtomicUsize` step counter for file logging. Each step resolves its `agent` name to the matching `AgentConfig`, creates a `HermesClient` on-the-fly, then goes through: pre-hooks → template rendering → context-aware instructions construction → write `.prompt` file → log request body (via `write_request_log_file` and `HermesClient::build_request_body`) → Hermes API call → overwrite `.log` file with full exchange (via `write_log_file`) → post-hooks. The request body is logged before the API call so it is preserved even if the call fails. The `instructions` field sent to the Hermes API is dynamically constructed based on the workflow's `GitConfig`: when `git.clone` or `git.worktree` is enabled, instructions include the workspace directory path and an explicit `cd` directive (`All work is in: {path}. Always run cd {path}...`); when both are disabled, the `instructions` field is omitted from the request entirely (the step name is already passed as the prompt `input`). Fail-fast: the first error stops the entire workflow. `RunnerError` enum wraps errors from template rendering (`Template`), hook validation (`Hook`), Hermes API calls (`Harness`), step execution (`Execution`), and unknown agent references (`UnknownAgent`). Pre-hook failure prevents step execution; post-hook failure marks the step as failed. The `Dispatcher` passes `agents.clone()` and the API key to `WorkflowRunner::new` so each workflow can target different agents per step. Integration tests use an in-process mock axum server on a random port to simulate the Hermes API.

## CLI Arguments

```
yoke [OPTIONS]
yoke webhooks <SUBCOMMAND>

Options:
  --config <PATH>       Path to config.toml (default: config.toml)
  --workflows <DIR>      Directory containing workflow TOML files (default: ./workflows)
  --host <ADDR>          Server bind address (overrides config.toml)
  --webhook-host <HOST>  External hostname for webhook URLs (overrides config.toml webhook_host)
  --port <PORT>          Server listen port (overrides config.toml)

Webhook subcommands:
  webhooks list              List webhooks for all configured repositories
  webhooks add               Add or update webhooks on all configured repositories based on workflow triggers
  webhooks remove            Remove Yoke webhooks (matched by URL) from all configured repositories
```

Note: `[runtime].max_concurrent`, `[runtime].workdir`, and `platform` are set in `config.toml` only (no CLI flags).

- **`webhooks` subcommand**: When a subcommand is provided, yoke handles it and exits without starting the server. The `webhooks` command reads `platform`, `repos`, and `gitlab_url` from `config.toml` and uses `GITHUB_TOKEN` or `GITLAB_TOKEN` env vars for authentication. It dispatches to `GitHubWebhookClient` or `GitLabWebhookClient` based on the platform. `webhooks list` prints all webhooks for each configured repo (with secret redacted). `webhooks remove` removes all webhooks whose URL matches the Yoke server URL (`https://{host}:{port}/webhook`) from each configured repo, returning a `RemoveSummary` with deleted/not_found/errors counts. `webhooks add` loads workflow TOML files, derives required events from trigger types via `Workflow::derive_required_events()`, and idempotently creates or updates webhooks on each configured repo, returning an `AddSummary` with created/updated/skipped/errors counts.

## Known Trigger Types

GitHub triggers: `github_issue_assigned`, `github_issue_comment_mention`, `github_pull_request_review`, `github_pull_request_comment_mention`

GitLab triggers: `gitlab_issue_assigned`, `gitlab_issue_mention`, `gitlab_merge_request_review`, `gitlab_merge_request_review_comment`

## Dependencies

| Crate | Purpose |
|---|---|
| `axum` | HTTP server framework |
| `clap` | CLI argument parsing with derive macros |
| `serde` | Deserialize/serialize config and workflow structs |
| `serde_json` | JSON serialization for health endpoint response |
| `tokio` | Async runtime (full features) |
| `toml` | Parse config.toml and workflow .toml files |
| `tower` | Service abstraction (ServiceExt for tests) |
| `tower-http` | HTTP middleware (body limit, tracing, CORS) |
| `tracing` | Structured logging |
| `tracing-subscriber` | Log subscriber with env-filter and local-time support |
| `time` | Time formatting for HH:MM:SS timestamps in logs |
| `url` | Parse and validate URLs in agent config |
| `shellexpand` | Expand `~` in workdir paths |
| `hmac` | HMAC-SHA256 computation for GitHub webhook signature verification |
| `sha2` | SHA-256 digest (used with hmac) |
| `hex` | Hex encoding for HMAC signature comparison |
| `subtle` | Constant-time comparison to prevent timing attacks on webhook secrets/tokens |
| `git2` | libgit2 bindings for git repository operations (clone, pull, worktree, auth) |
| `thiserror` | Derived error types (Display, Error) for PersistenceError and other enums |
| `reqwest` | HTTP client for Hermes Agent API requests (with JSON feature) |
| `tempfile` | Temporary directories for unit tests (dev dependency) |

## Running Tests

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

Environment variable validation tests (`test_validate_env_vars_*`) use a static `ENV_MUTEX` to serialize access to global env state, preventing race conditions when tests run in parallel. No `--test-threads=1` or env var workarounds are needed.

### Integration tests

The `tests/dispatcher_tests.rs` file contains integration tests that exercise the dispatcher module end-to-end. These tests import from `yoke::dispatcher` (requires the library target in `src/lib.rs`).

The `tests/runner_tests.rs` file contains integration tests that exercise the workflow runner end-to-end. These tests use an in-process mock axum server to simulate the Hermes API and import from `yoke::runner` (requires the library target in `src/lib.rs`).

Runner tests cover:
- Two-step workflow execution with sequential API calls
- Template variable substitution (`{{variable}}`) in prompt templates
- Unknown variable errors cause step failure
- Pre-hook failure prevents step execution
- Post-hook failure marks step as failed
- Fail-fast on first step error
- Pre and post hooks both passing
- Steps execute in defined order
- Hook failure between steps stops the workflow

Dispatcher tests cover:
- Full dispatch flow: message sent via channel → `run()` dispatches → `on_workflow_complete` called
- Duplicate event rejection: second event with the same dedup key is skipped
- Concurrency limits: `max_concurrent` semaphore blocks excess events
- `active_count` tracking: observed in concurrent scenarios
- Graceful shutdown: dropping the sender closes the channel, `run()` exits cleanly
- Drain: in-flight events finish before `run()` returns
- Persistence: completed and failed events are written to `completed.json` / `failed.json`
- GitLab event dispatched: GitLab trigger events are handled correctly
- Multiple different events: distinct events process independently
- Unlimited throughput (`max_concurrent = 0`): 500 events processed without throttling
- Concurrency stress (`max_concurrent = 4`): 50 events complete through semaphore
- Failure state transitions: `on_workflow_complete(Err)` moves key to `permanently_failed`
- Permits released: semaphore permits return after task completion, and `active_count` returns to 0 after all `spawn_workflow` tasks complete (both limited and unlimited concurrency)
- Corrupted JSON persistence: `load_persistence` returns empty sets for corrupted `completed.json` and `failed.json`
- Both files corrupted: both sets are empty, in_flight always empty
- Atomic write failure: writing to a read-only directory fails without corrupting existing file
- Semaphore stress: high-concurrency with bounded permits

The `tests/webhooks_tests.rs` file contains integration tests for the webhooks CLI command handlers. These tests use `mockito` to mock the GitHub API and verify the behavior of `webhooks_list`, `webhooks_remove`, and `webhooks_add` against various scenarios (empty repos, API errors, existing hooks, new hooks, URL matching).
## Logging

Note: This section covers application-level structured logging (console/stderr output via the `tracing` crate). This is distinct from the workflow step audit files written by `src/file_log.rs`, which record per-step HTTP exchanges to disk.

Yoke uses the `tracing` crate for structured logging. The subscriber is initialized in `main.rs` with `env-filter` support (controlled by `RUST_LOG`) and `HH:MM:SS` local-time timestamps.

**Directive:** Include logs at high-level operation boundaries and within error paths. When adding new functionality, add `tracing::info!` at the completion of significant operations and `tracing::debug!`/`tracing::trace!` for internal detail.

### Log Level Contract

| Level | When to use |
|-------|-------------|
| `error!` | Unexpected failures requiring attention |
| `warn!` | Unexpected conditions the system is continuing through |
| `info!` | Significant, operator-relevant events (service start/stop, operation completion). Low volume, always useful |
| `debug!` | Developer-relevant detail for debugging. Safe to enable in production during incidents |
| `trace!` | Inner-loop, per-iteration detail. Only enabled during intense tracing sessions |

### Spans and `#[instrument]`

Use `#[instrument]` on functions to automatically carry context through the call stack. This is preferred over threading context into every log line.

```rust
#[instrument(skip(self, body), fields(platform = ?self.platform))]
async fn handle_webhook(&self, ...) -> Result<(), WebhookError> {
    // All logs here automatically carry platform and any recorded fields
}
```

### Event Context

Most activity in yoke relates to an Event with a unique `event_id`. Every log emitted while processing an event must include `event_id`. The preferred approach is `#[instrument]` on the event-handling function with `event_id` as a span field:

```rust
#[instrument(skip_all, fields(event_id = %msg.event.event_id, repo = %msg.event.repo_path))]
async fn spawn_workflow(&self, msg: DispatchMessage) {
    // All logs in this span carry event_id and repo automatically
}
```

### Message Style Conventions

All `tracing` log messages must follow these conventions:

1. **Capitalize the first letter** — Message strings start with an uppercase letter: `"Dispatcher run loop started"`, not `"dispatcher run loop started"`.
2. **"Failed to X" over "Error X"** — Use `"Failed to create webhook client"`, not `"Error creating webhook client"`. This clarifies that the operation didn't succeed, rather than naming the category.
3. **No trailing ellipsis** — Avoid `...` at the end of messages. Prefer `"Waiting for in-flight workflows to complete"` over `"waiting for in-flight workflows to complete..."`. Exception: ellipsis is acceptable in user-facing CLI output that implies an ongoing process.
4. **Named fields over format strings** — Use structured tracing fields instead of `format!()` interpolation:
   ```rust
   // ✅ Preferred — structured field
   tracing::info!(workflow_count = workflows.len(), "Configuration and workflow(s) loaded");
   
   // ❌ Avoid — format string interpolation
   tracing::info!("Configuration and {} workflow(s) loaded", workflows.len());
   ```
5. **Filenames over full paths** — Log workflow names as just the filename (e.g. `deploy.yml`) rather than the full filesystem path (e.g. `/etc/yoke/workflows/deploy.yml`). Use `Path::file_name()` to extract the filename:
   ```rust
   let workflow_name = Path::new(&path).file_name().unwrap_or_default().to_string_lossy();
   tracing::info!(workflow = %workflow_name, "Running matching workflow");
   ```
6. **Include `repo` and `event_id` fields** — All workflow execution log messages (spawning, running, completing, failing) must include `repo` and `event_id` structured fields for log correlation:
   ```rust
   tracing::info!(
       workflow = %workflow_name,
       repo = %event.repo_path,
       event_id = %event_id,
       "Running matching workflow"
   );
   ```

### No `println!` / `eprintln!`

The codebase must not contain `println!` or `eprintln!` calls. All output goes through `tracing` macros. CLI output (e.g. `yoke webhooks list`) uses `tracing::info!` so it is captured by the subscriber and respects `RUST_LOG` filtering.

### Controlling Log Output

```bash
# Default: info level
cargo run

# Debug level for the yoke crate
RUST_LOG=yoke=debug cargo run

# Only warnings and errors
RUST_LOG=yoke=warn cargo run

# Trace level for intense debugging
RUST_LOG=yoke=trace cargo run
```
