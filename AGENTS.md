# Yoke — Agent Notes

## Project Structure

```
src/
  lib.rs       — Library root (pub mod re-exports for integration tests)
  main.rs      — CLI entrypoint (loads config, loads workflows, validates agents & triggers, starts server)
  cli.rs       — CLI argument parsing (clap derive)
  config.rs    — Configuration parsing, validation, and error types
  dispatcher.rs — Concurrency control (Dispatcher + Semaphore), deduplication (DedupSets, SharedDedupSets), persistence, and workspace directory management
  harness.rs   — Hermes API client harness (HermesClient, request/response types, StepResult, error handling)
  logging.rs   — Workflow step logging: write `.prompt` and `.log` files per agent step
  reload.rs    — File watcher for workflow hot-reload (notify crate, debouncing, ReloadMessage types, WorkflowState, reload_workflows)
  server.rs    — axum HTTP server with health, readiness, and unified webhook endpoint
  workflow.rs  — Workflow TOML parsing, validation, and error types
  git.rs        — Git repository operations (clone, pull, worktree, auth callbacks, dirty-check)
  github_api.rs — GitHub REST API client (GitHubClient, Webhook, WebhookConfig, WebhookOrchestrationSummary, list/create/update/delete webhooks, find_webhook_by_url, ensure_webhook, orchestrate_webhooks, error types)
  template.rs  — Template rendering with `{{variable}}` substitution and validation
  hooks.rs     — Hook definitions (FileNotEmpty, FileContains) and run_hook dispatcher
  runner.rs    — Workflow runner: sequential step execution with template vars, hooks, and fail-fast
  webhook/     — Webhook handling modules
    mod.rs       — Shared types (TriggerEvent, WebhookError) and dispatch to platform handler
    github.rs   — GitHub webhook: HMAC-SHA256 verification, event parsing, trigger mapping
    gitlab.rs   — GitLab webhook: token verification, payload parsing, event mapping
    gitlab_api.rs — GitLab REST API client (GitLabClient, GitLabWebhook struct, list_webhooks with pagination, find_webhook_by_url, error types)
tests/
  dispatcher_tests.rs — Integration tests for dispatcher (full dispatch flow, dedup, concurrency, shutdown, persistence)
  git_integration_tests.rs — Integration tests for git module (clone, pull, worktree, dirty-check with local repos)
  harness_tests.rs   — Integration tests for harness (serialization, response parsing, error file, multi-instance)
  reload_tests.rs    — Integration tests for reload module (file detection, debouncing, .toml filtering)
  reload_integration_tests.rs — Integration tests for hot-reload (add workflow, invalid file preserves state, file removal)
  runner_tests.rs   — Integration tests for workflow runner (step execution, template vars, hooks, fail-fast)
  shutdown_tests.rs  — Integration tests for graceful shutdown (SIGINT/SIGTERM, drain, state persistence)
```

## Key Design Decisions

- **Library + binary crate**: Yoke is both a library and a binary. `src/lib.rs` re-exports all modules as `pub` so integration tests (`tests/`) can import from `yoke::`. `src/main.rs` uses `use yoke::` imports instead of inline `mod` declarations.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **CLI argument parsing**: Uses `clap` with derive macros. `--config` and `--workflows` have defaults; `--host` and `--port` override values from `config.toml`.
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes, trigger types) is done in `Config::validate()` / `Workflow::validate()`.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand, AgentResolution, EnvVar) with `Display` and `Error` impls.
- **Agent resolution**: `resolve_agents(config, workflows)` validates that every `step.agent` in every workflow matches a configured `[[agents]]` name. Returns `ConfigError::AgentResolution` with step name, workflow path, and missing agent.
- **Environment variable validation**: `validate_env_vars(platform)` checks required env vars at startup. `HERMES_API_KEY` and `WEBHOOK_SECRET` are always required; `GITHUB_TOKEN` is required when `platform = "github"`, `GITLAB_TOKEN` when `platform = "gitlab"`. Returns `ConfigError::EnvVar` with a descriptive message.
- **`WEBHOOK_SECRET` env override**: The `WEBHOOK_SECRET` env var overrides the `config.toml` `server.webhook_secret` value at startup.
- **GitHub API client** (`src/github_api.rs`): `GitHubClient` struct wraps `reqwest::Client` with Bearer token authentication. Public API: `list_webhooks(owner, repo)` with transparent pagination (follows `Link` rel="next" headers), `create_webhook(owner, repo, config)` (POST to `/repos/{owner}/{repo}/hooks`), `update_webhook(owner, repo, webhook_id, config)` (PATCH to `/repos/{owner}/{repo}/hooks/{id}`), `delete_webhook(owner, repo, webhook_id)` (DELETE to `/repos/{owner}/{repo}/hooks/{id}`). `WebhookConfig` struct (`url`, `secret`, `events`) is passed to create/update. `find_webhook_by_url(webhooks, url)` searches a webhook list by URL for idempotency checks. `ensure_webhook(owner, repo, config)` is an idempotent orchestrator that lists existing webhooks, finds by URL, and either updates the existing webhook or creates a new one — returning a `WebhookOrchestrationSummary`. `orchestrate_webhooks(repos, config)` iterates over multiple `(owner, repo)` pairs, calling `ensure_webhook` for each and aggregating results into a single summary with `created`, `updated`, and `skipped` counts. Error mapping: HTTP 401→`Unauthorized`, 404→`NotFound`, 403→`RateLimited`, 201→success (create), 200→success (update, list), 204→success (delete), others→`ApiError`. Unit tests use `mockito` for HTTP mocking and cover: list (success, empty, pagination, 401, 404, 403), create (success 201, conflict 422), update (success 200, not found 404), delete (success 204, not found 404), `find_webhook_by_url` helper, `ensure_webhook` (creates new, updates existing), and `orchestrate_webhooks` (multi-repo with mixed create/update).
- **GitLab API client** (`src/webhook/gitlab_api.rs`): `GitLabClient` struct wraps `reqwest::Client` with Private-Token authentication. Exposes `list_webhooks(project_id)` that handles GitLab API pagination transparently (follows `Link` rel="next" headers). Supports self-hosted GitLab via configurable `base_url` (default: `https://gitlab.com/api/v4`). Also provides `find_webhook_by_url` for idempotency checks. Error mapping: HTTP 401→`Unauthorized`, 404→`NotFound`, others→`ApiError`. Unit tests use `mockito` for HTTP mocking and cover: success (single page), empty list, pagination (two pages), auth failure (401), not found (404), server error (500), `find_webhook_by_url` (found, not found, empty), and client construction (default/custom base URL).
- **Trigger platform validation**: After loading config and workflows, `validate_triggers()` checks that each workflow's trigger type prefix matches the configured platform. GitLab triggers (`gitlab_*`) on a GitHub platform (and vice versa) cause a hard exit with a clear error.
- **`TriggerType` enum**: Typed representation of known trigger types (4 GitHub, 4 GitLab). Each variant carries its required filter fields per Appendix A. `TriggerType::from_trigger()` converts a `Trigger` struct; `TriggerType::platform()` returns the owning platform; `TriggerType::label()` returns the string identifier used in workflow TOML files.
- **`WorkflowError` enum**: Typed errors (Io, Parse, Validation) with `Display` and `Error` impls. Parse/Validation errors include the file path for clear diagnostics.
- **`Workflow.path` field**: Each `Workflow` carries its source file path (populated by `load_workflows`), used for agent resolution error reporting.
- **Hooks module** (`src/hooks.rs`): Defines `Hook` enum with `FileNotEmpty { path }` and `FileContains { path, text }` variants, plus `run_hook()` dispatcher and `HookError` error type. Hooks are configured per-step in TOML using internally-tagged representation (`type = "file_not_empty"` or `type = "file_contains"`). The `Step` struct in `workflow.rs` re-exports `Hook` via `pub use crate::hooks::Hook`. Hook failures return clear error messages: `"File 'X' is empty"`, `"File 'X' not found"`, `"File 'X' does not contain 'Y'"`.
- **Template renderer**: `template::render()` does `{{var}}` substitution, returning `Result<_, TemplateError>` for unknown variables, malformed syntax, and empty templates.
- **HTTP server**: `src/server.rs` uses axum with `tower-http` middleware. Three endpoints: `/health` (liveness, returns `{"status":"ok"}`), `/ready` (readiness, returns 200 — always ready for now), `/webhook` (POST — dispatches to platform-specific handler based on `platform` config). `RequestBodyLimitLayer` enforces `max_body_size` from config. `TraceLayer` provides structured HTTP request logging.
- **Signal handler** (`src/main.rs`): `setup_signal_handler` creates a tokio task that listens for SIGINT and SIGTERM using `tokio::signal::unix`. On the first signal, it sends `true` on a `watch::Sender<bool>` channel which propagates to the HTTP server and dispatcher via the corresponding `watch::Receiver`. On a second signal, it calls `process::exit(1)` for immediate termination. The signal handler also has a 60-second safety timeout — if no second signal arrives, the task exits cleanly after the main runtime has shut down.
- **Graceful shutdown flow**: When the signal handler sends `true` on the watch channel: (1) the axum HTTP server's `with_graceful_shutdown` future detects the change and stops accepting new connections, (2) the dispatcher's `run_with_drain` loop stops consuming from the mpsc channel, (3) the dispatcher waits up to `config.runtime.drain_timeout_secs` (default: 30s) for in-flight workflows to complete (checking `active_count` every 100ms), (4) state is persisted via `Dispatcher::persist_state()` which writes `completed.json` and `failed.json` atomically, (5) the process exits. A second SIGINT/SIGTERM during the drain phase forces an immediate `process::exit(1)`.
- **Configurable drain timeout**: `config.runtime.drain_timeout_secs` (default: 30) controls how long the dispatcher waits for in-flight workflows to complete during shutdown. Set to 0 for instant (non-graceful) shutdown.

- **Webhook dispatch**: The server uses `WebhookHandler` (in `webhook/mod.rs`) which holds the platform config, webhook secret, and an mpsc sender for dispatching verified events. The `AppState` struct contains a `WebhookHandler` instance. The webhook endpoint handler delegates to `WebhookHandler::handle_webhook()`, which authenticates the request, parses the payload, maps it to a `TriggerEvent`, and sends it over the mpsc channel. Returns `Ok(())` on success or a `WebhookError` variant. When the dispatcher channel is closed (receiver dropped), returns `InternalError` → HTTP 503.
- **Git operations module**: `src/git.rs` provides git repository management for the dispatcher pipeline. Public API: `sanitize_branch_name()`, `build_clone_url()`, `GitAuth` (credential callbacks for GitHub/GitLab tokens), `clone_repo()`, `pull_repo()`, `create_worktree()`, `remove_worktree()`, `has_uncommitted_changes()`. Uses the `git2` crate (libgit2 bindings). Token-based auth is wired through `GitAuth` callbacks — GitHub tokens go in the `x-access-token` header, GitLab tokens in the `PRIVATE-TOKEN` header. Branch names are sanitized to `[a-zA-Z0-9._-]` with whitespace collapsed to `-`. Worktrees use sanitized branch names for the administrative directory and support creating from existing branches or branching off HEAD.

- **GitHub webhook handler**: `src/webhook/github.rs` provides HMAC-SHA256 signature verification (`verify_github_signature`), JSON payload parsing (`parse_github_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_github_webhook` function orchestrates the full flow.
- **GitLab webhook handler**: `src/webhook/gitlab.rs` provides constant-time token verification (`verify_gitlab_token`), JSON payload parsing (`parse_gitlab_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_gitlab_webhook` function orchestrates the full flow.
- **Constant-time comparison**: Both handlers use `subtle::ConstantTimeEq` to prevent timing attacks — GitHub for HMAC signatures, GitLab for token comparison.
- **`WebhookError` enum**: Shared error type (Unauthorized, BadRequest, NoMatchingTrigger, InternalError) in `webhook/mod.rs`, used by `WebhookHandler::handle_webhook()`. `InternalError` is returned when the dispatcher channel is closed, mapping to HTTP 503 Service Unavailable.
- **File watcher / Hot-reload** (`src/reload.rs`): Monitors the `--workflows` directory for `.toml` file changes using the `notify` crate. `setup_file_watcher(workflows_dir, tx)` creates a `notify::RecommendedWatcher`, a bridge thread (sync→async channel adapter), and a debouncing tokio task. The `FileWatcher` handle keeps the watcher alive — dropping it stops the watcher. Debouncing: 500ms after the last event, a single `ReloadMessage` (`FileChanged { path }` or `FileRemoved { path }`) is sent on the async channel. Non-`.toml` files are filtered out in the sync callback before entering the pipeline. The bridge thread converts the synchronous `notify` callback into async events via `blocking_send`. The debounce loop is fully async (`tokio::time::timeout` for the debounce window), avoiding blocking the runtime. `WorkflowState` wraps an `ArcSwap<Vec<(String, Workflow)>>` for lock-free atomic state swaps. `reload_workflows(workflows_dir, config)` performs a full re-load and validation cycle (TOML parsing, agent resolution, trigger platform validation). On success, `WorkflowState::update()` atomically swaps the in-memory workflow set. On validation failure, the error is logged and the previous state is preserved. The reload handler in `main.rs` listens for `ReloadMessage` events and calls `reload_workflows` + `WorkflowState::update` on each notification.
- **`TriggerEvent` struct**: Shared webhook result type in `webhook/mod.rs` with `trigger_type: TriggerType`, `repo_path`, and `event_id` fields. Sent to the dispatcher via the mpsc channel in `WebhookHandler`.
- **`WebhookHandler` struct**: Holds `platform`, `secret`, and `sender: mpsc::Sender<TriggerEvent>`. Created in `run_server()` with a bounded channel and passed to `AppState`. Derives `Clone`.
- **`AppState` struct**: Contains `webhook_handler: WebhookHandler` and `dispatcher: Dispatcher`. Derives `Clone` for axum state sharing. The `dispatcher` field provides concurrency control (via `tokio::Semaphore`) and deduplication state (`SharedDedupSets`).
- **Dispatcher and concurrency control** (`src/dispatcher.rs`): The `Dispatcher` struct wraps `SharedDedupSets` and an optional `tokio::Semaphore` to coordinate concurrency limiting and deduplication for webhook event processing. When `max_concurrent > 0`, the dispatcher holds a `Semaphore` that caps simultaneous workflow executions; permits are acquired via `acquire_permit()` (returning `Option<OwnedSemaphorePermit>`) or the convenience method `run_with_permit()` which holds the permit for the future's lifetime and releases it on drop (RAII pattern). When `max_concurrent == 0`, the semaphore is `None` and no limiting is applied. An `AtomicUsize` counter tracks active permits for observability. The `Dispatcher` is `Clone` (cheap via `Arc` clones) and is stored in `AppState` for sharing across axum handlers.
- **Dispatcher deduplication** (`src/dispatcher.rs`): Three-set `DedupSets` tracks event lifecycle states (`in_flight`, `completed`, `permanently_failed`). Events are identified by dedup keys formatted as `{owner}/{repo}/{event_id}`, where `event_id` varies by event type: issue number for issue events, `{pr_number}_review-{review_id}` for PR reviews, `{pr_number}_comment-{comment_id}` for PR review comments, and issue number for issue comment mentions. `SharedDedupSets` (`Arc<RwLock<DedupSets>>`) provides thread-safe async access. An event is considered a duplicate if its key appears in *any* of the three sets. State transitions: `mark_in_flight` → `mark_completed` (success) or `mark_failed` (permanent failure); `remove_in_flight` allows retry on transient failures. The `extract_event_id` function maps `TriggerEvent` fields to dedup key components based on `TriggerType`.
- **Dedup persistence** (`src/dispatcher.rs`): `FailedEntry` struct records permanently failed events with `{key, timestamp, error}`. `PersistenceError` enum handles IO and JSON errors from file operations. `load_dedup_file` deserializes JSON files (returns `NotFound` for missing, `Json` for corrupted). `save_dedup_file` uses atomic writes — writes to `.json.tmp`, then `rename` to target — to prevent data corruption on crash. `DedupSets::persist_completed` saves the `completed` set to `completed.json`. `DedupSets::persist_failed` appends a `FailedEntry` to `failed.json` (load-append-save pattern; JSON arrays require full rewrite). `load_persistence` reads `completed.json` and `failed.json` from the work directory at startup, treating missing files as empty sets and logging warnings for corrupted ones. `in_flight` is always empty on load (transient state).
- **Hermes API harness** (`src/harness.rs`): `HermesClient` encapsulates a `reqwest::Client`, `base_url`, and `api_key` for making authenticated POST requests to the Hermes Agent API `/v1/responses` endpoint. `execute_step(instructions, input)` builds a `HermesRequest { instructions, input, store: true }`, sends it via `POST {base_url}/v1/responses` with `Authorization: Bearer *** and parses the response into a `HermesResponse`. Response parsing filters `output` content blocks for `type == "output_text"` and joins their text with newlines. Non-2xx responses write the status code and body to a `.error` file and return `HarnessError::Api`. `HarnessError` has three variants: `Http` (network/request errors), `Api` (non-2xx status with status code and body), and `Io` (file write errors for `.error`). `execute_step_with_error_path` accepts an optional `Path` for the error file (used in tests). `HermesRequest`, `HermesResponse`, and `ContentBlock` derive `Serialize`/`Deserialize` for JSON round-tripping; `ContentBlock` uses `#[serde(rename = "type")]` for the `block_type` field.
- **StepResult struct** (`src/harness.rs`): `StepResult` captures the output of a single agent step execution. Fields: `extracted_message` (the text from `output_text` content blocks), `raw_request` (the full JSON request body sent to the API), and `raw_response` (the full JSON response body received). Both `execute_step` and `execute_step_with_error_path` return `Result<StepResult, HarnessError>` instead of `Result<String, HarnessError>`, enabling audit logging of the full HTTP exchange per step.
- **Workflow logging** (`src/logging.rs`): `write_prompt_file(step_num, step_name, prompt, workspace_dir)` writes a rendered prompt template to `{workspace_dir}/{step_num:02}_{step_name}.prompt`. `write_log_file(step_num, step_name, request, response, extracted_message, workspace_dir)` writes a human-readable log of the full HTTP exchange to `{workspace_dir}/{step_num:02}_{step_name}.log`, with sections for `REQUEST:`, `RESPONSE:`, and `FINAL MESSAGE:`. Both functions create the workspace directory if it doesn't exist. File naming uses zero-padded two-digit step numbers (e.g., `00_Start.log`, `01_Analyze.prompt`).
- **Workspace directory** (`src/dispatcher.rs`): `workspace_dir(workdir, owner, repo, event_id)` constructs the per-event workspace path `{workdir}/{owner}/{repo}/{event_id}/` per the architecture design doc (Section 11). The dispatcher creates this directory before spawning the workflow task and writes a step-0 `Start.log` file to record the trigger type and event metadata.
- **Workflow runner** (`src/runner.rs`): `WorkflowRunner` orchestrates sequential execution of a `Workflow`. Each step goes through: pre-hooks → template rendering → Hermes API call → post-hooks. Fail-fast: the first error stops the entire workflow. `RunnerError` enum wraps errors from template rendering (`Template`), hook validation (`Hook`), Hermes API calls (`Harness`), and step execution (`Execution`). Pre-hook failure prevents step execution; post-hook failure marks the step as failed. Integration tests use an in-process mock axum server on a random port to simulate the Hermes API.

## CLI Arguments

```
yoke [OPTIONS]

Options:
  --config <PATH>       Path to config.toml (default: config.toml)
  --workflows <DIR>      Directory containing workflow TOML files (default: .)
  --host <ADDR>          Server bind address (overrides config.toml)
  --port <PORT>          Server listen port (overrides config.toml)
```

Note: `[runtime].max_concurrent`, `[runtime].workdir`, and `platform` are set in `config.toml` only (no CLI flags).

## Known Trigger Types

GitHub triggers: `github_issue_assigned`, `github_issue_comment_mention`, `github_pull_request_review`, `github_pull_request_review_comment`

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
| `tracing-subscriber` | Log subscriber with env-filter support |
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
- Permits released: semaphore permits return after task completion
- Corrupted JSON persistence: `load_persistence` returns empty sets for corrupted `completed.json` and `failed.json`
- Both files corrupted: both sets are empty, in_flight always empty
- Atomic write failure: writing to a read-only directory fails without corrupting existing file
- Semaphore stress: high-concurrency with bounded permits