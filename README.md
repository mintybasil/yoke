# Yoke

A Rust daemon that receives webhook events from GitHub or GitLab and runs multi-step agent workflows through the Hermes Agent REST API.

## Quick Start

```bash
# Build
cargo build

# Run tests (unit + integration)
cargo test

# Lint
cargo fmt --check
cargo clippy -- -D warnings

# Run with defaults (loads config.toml from current directory)
cargo run

# Run with custom config and workflows directory
cargo run -- --config /path/to/config.toml --workflows /path/to/workflows

# Override server host and port
cargo run -- --host 127.0.0.1 --port 9000
```

## CLI Arguments

```
yoke [OPTIONS]

Options:
  --config <PATH>       Path to config.toml (default: config.toml)
  --workflows <DIR>      Directory containing workflow TOML files (default: .)
  --host <ADDR>          Server bind address (overrides config.toml)
  --port <PORT>          Server listen port (overrides config.toml)
  -h, --help             Print help
  -V, --version          Print version
```

CLI `--host` and `--port` flags override the corresponding values in `config.toml`. The `[runtime].max_concurrent`, `[runtime].workdir`, and `platform` settings are configured in `config.toml` only and cannot be overridden from the command line.

## Configuration

Yoke reads configuration from a `config.toml` file. The default path is `config.toml` in the current directory; override with `--config`.

### config.toml

```toml
# Platform: "github" or "gitlab" — determines webhook handler and event types
platform = "github"

# Repos to monitor — shared across all workflows
repos = [
    { owner = "example-corp", repo = "backend-service" },
    { owner = "example-corp", repo = "frontend-app" },
]

# Named agent instances (Hermes API configs)
[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"

# Runtime settings
[runtime]
max_concurrent = 2       # max concurrent workflows (0 = unlimited)
workdir = "~/.yoke"      # runtime data directory (supports ~ expansion)

# Server settings
[server]
host = "0.0.0.0"
port = 8644
webhook_secret = "your-webhook-secret"  # GitHub HMAC key or GitLab token
max_body_size = 1048576                  # 1MB default

# GitLab-specific (only when platform = "gitlab")
# gitlab_url = "https://gitlab.mycompany.com"
```

### Required Fields

- `platform` — must be `"github"` or `"gitlab"`
- `agents` — at least one agent with a unique `name` and valid `base_url` (http/https)
- `server.webhook_secret` — webhook authentication key

### Defaults

| Field | Default |
|---|---|
| `repos` | `[]` (empty) |
| `runtime.max_concurrent` | `0` (unlimited) |
| `runtime.workdir` | `"~/.yoke"` |
| `server.host` | `"0.0.0.0"` |
| `server.port` | `8644` |
| `server.max_body_size` | `1048576` |
| `gitlab_url` | `https://gitlab.com` |

### Validation

The application fails fast on configuration errors:

- Missing required fields produce clear error messages
- Invalid TOML syntax produces parse errors
- Invalid `platform` value (not `github` or `gitlab`) is rejected
- Invalid URL in `agents[].base_url` is rejected
- Duplicate agent names are rejected
- Tilde (`~`) in `workdir` is expanded to the home directory
- Agent resolution: every `step.agent` in workflow files must match a configured `[[agents]]` name — unknown agents produce a clear error with the step name, workflow file, and missing agent name

### Environment variables

Yoke validates required environment variables at startup. Missing variables cause an immediate exit with a clear error message.

| Variable | Purpose | Required |
|---|---|---|
| `HERMES_API_KEY` | Bearer token for Hermes REST API | Always |
| `WEBHOOK_SECRET` | Webhook authentication key (overrides `config.toml` `server.webhook_secret`) | Always |
| `GITHUB_TOKEN` | GitHub auth for git clone/pull | When `platform = "github"` |
| `GITLAB_TOKEN` | GitLab auth for git clone/pull | When `platform = "gitlab"` |

## Workflow Files

Yoke loads workflow definitions from `.toml` files in a directory (default: current directory; override with `--workflows`). Each file defines a trigger, git configuration, and a sequence of steps.

### workflow.toml example

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "alice"

[git]
clone = true
worktree = true
default_branch = "main"

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan the issue"

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan the issue"
post_hooks = [{ type = "file_not_empty", path = "plan.md" }]

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = "Read the plan and implement it."
```

### Workflow fields

| Field | Purpose | Default |
|---|---|---|
| `[trigger].type` | Event type (e.g. `github_issue_assigned`) | required |
| `[trigger].assigned_to` | Filter by assignee | optional |
| `[trigger].mentioned_user` | Filter by mentioned user | optional |
| `[trigger].allowed_users` | Filter by user list | optional |
| `[git].clone` | Whether to git clone the repo | `true` |
| `[git].worktree` | Whether to create a per-event worktree | `true` |
| `[git].default_branch` | Branch for clone/worktree base | `"main"` |
| `[[steps]].name` | Human-readable step label | required |
| `[[steps]].agent` | Agent name from `config.toml` | required |
| `[[steps]].prompt_template` | `{{variable}}` template | required |
| `[[steps]].pre_hooks` | Hooks to check before step | none |
| `[[steps]].post_hooks` | Hooks to check after step | none |

### Hook types

Hooks are inline TOML tables with a `type` field and hook-specific parameters:

```toml
pre_hooks = [{ type = "file_not_empty", path = "plan.md" }]
post_hooks = [{ type = "file_contains", path = "plan.md", text = "implementation" }]
```

| Hook | Fields | Description |
|---|---|---|
| `file_not_empty` | `path` | Checks that a file exists and has non-zero content |
| `file_contains` | `path`, `text` | Checks that a file contains a specific string |

A hook failure stops the workflow and produces a clear error message identifying the file (and text, for `file_contains`) that failed validation.

### Known trigger types

Trigger types are platform-specific. Their prefix must match the `platform` setting in `config.toml`:

**GitHub triggers** (require `platform = "github"`):

| Trigger Type | Event |
|---|---|
| `github_issue_assigned` | Issue assigned to a user |
| `github_issue_comment_mention` | Comment on an issue mentions a user |
| `github_pull_request_review` | Pull request review submitted |
| `github_pull_request_review_comment` | Pull request review comment |

**GitLab triggers** (require `platform = "gitlab"`):

| Trigger Type | Event |
|---|---|
| `gitlab_issue_assigned` | Issue assigned to a user |
| `gitlab_issue_mention` | Note on an issue mentions a user |
| `gitlab_merge_request_review` | Note on a merge request |
| `gitlab_merge_request_review_comment` | DiffNote on a merge request |

### Workflow validation

Workflow files are validated at load time:

- `trigger.type` must be non-empty and one of the known trigger types
- At least one step is required
- Every step must have a non-empty `prompt_template`
- Parse errors include the file path for easy debugging

### Trigger platform validation

At startup, Yoke verifies that every workflow's trigger type matches the configured platform:

- GitHub triggers (prefixed with `github_`) are only valid when `platform = "github"`
- GitLab triggers (prefixed with `gitlab_`) are only valid when `platform = "gitlab"`
- A mismatch causes a hard exit with a clear error message, e.g.: `Workflow 'gitlab-plan.toml' has trigger 'gitlab_issue_assigned' but platform is 'github'`

### Template rendering

Step `prompt_template` fields use `{{variable}}` placeholder syntax:

| Syntax | Behavior |
|---|---|
| `{{key}}` | Substitute with the value of `key` |

Templates are validated at render time:

- **Unknown variable**: `{{unknown}}` returns `TemplateError::UnknownVariable`
- **Malformed syntax**: unclosed `{{var` or empty `{{}}` returns `TemplateError::SyntaxError`
- **Empty template**: whitespace-only results return `TemplateError::EmptyTemplate`

## HTTP Endpoints

Yoke starts an HTTP server on the configured `host:port`. The following endpoints are available:

| Method | Path | Description |
|---|---|---|
| GET | `/health` | Liveness check — returns `{"status":"ok"}` with 200 |
| GET | `/ready` | Readiness check — returns 200 (always ready; wired to dispatcher in future) |
| POST | `/webhook` | Platform-specific webhook receiver — GitHub: verifies `X-Hub-Signature-256` HMAC signature; GitLab: verifies `X-Gitlab-Token` header. Parses event payload, maps to internal trigger, and dispatches to the poller channel |

Request bodies larger than `[server].max_body_size` (default 1 MB) receive a **413 Payload Too Large** response.

#### Webhook response codes

| Code | Condition |
|---|---|
| 200 OK | Event processed successfully or no matching trigger (acknowledged but not processed) |
| 400 Bad Request | Malformed payload or missing event type header |
| 401 Unauthorized | Signature/token verification failed |
| 503 Service Unavailable | Dispatcher channel closed (poller not running or shut down) |

All HTTP requests are logged via `tower-http` tracing middleware.

### GitHub webhook verification

The `POST /webhook/github` endpoint uses HMAC-SHA256 signature verification:

1. The request must include an `X-Hub-Signature-256` header with the format `sha256=<hex-digest>`.
2. The hex digest is compared against an HMAC-SHA256 of the raw request body using the `webhook_secret` from config.
3. The request must include an `X-GitHub-Event` header specifying the event type (e.g. `issues`, `issue_comment`, `pull_request`).

If the signature is missing or fails verification, the server responds with **401 Unauthorized**. If the event type header is missing, the server responds with **400 Bad Request**. Known events that match a trigger type are logged, mapped, and dispatched to the poller channel. Unrecognized events respond with **200 OK** (acknowledged but not processed). If the dispatcher channel is closed, the server responds with **503 Service Unavailable**.

## Architecture

### Event deduplication

Yoke uses in-memory deduplication to prevent concurrent or repeated processing of the same webhook event. Each event is identified by a dedup key formatted as `{owner}/{repo}/{event_id}`, where the `event_id` component varies by event type:

| Trigger type | Event ID format |
|---|---|
| `github_issue_assigned` | `{issue_number}` |
| `github_issue_comment_mention` | `{issue_number}` |
| `github_pull_request_review` | `{pr_number}_review-{review_id}` |
| `github_pull_request_review_comment` | `{pr_number}_comment-{comment_id}` |
| `gitlab_issue_assigned` | `{issue_iid}` |
| `gitlab_issue_mention` | `{issue_iid}` |
| `gitlab_merge_request_review` | `{mr_iid}_note-{note_id}` |
| `gitlab_merge_request_review_comment` | `{mr_iid}_note-{note_id}` |

Three hash sets track event lifecycle states: `in_flight` (currently processing), `completed` (finished successfully), and `permanently_failed` (terminal failure). An event key present in any of these sets is considered a duplicate and will not be processed again. Events transition from `in_flight` to `completed` or `permanently_failed`; transient failures can use `remove_in_flight` to allow retries.

See [docs/Architecture Design.md](docs/Architecture%20Design.md) for the full system design.

### Concurrency limiting

Yoke can cap the number of workflows running simultaneously. The `[runtime].max_concurrent` setting (default: `0`, meaning unlimited) controls how many webhook events can be processed concurrently. When `max_concurrent > 0`, a `tokio::Semaphore` limits in-flight workflows — additional events wait for a permit before starting. When `max_concurrent == 0`, no semaphore is created and all events start immediately.

The `Dispatcher` struct holds both the concurrency semaphore and the deduplication sets, providing a single coordination point for event processing:

| Method | Behavior |
|---|---|
| `Dispatcher::acquire_permit()` | Returns `Some(OwnedSemaphorePermit)` when limited (blocks until available), `None` when unlimited |
| `Dispatcher::run_with_permit(fut)` | Convenience wrapper: acquires permit, runs future, releases permit (RAII) |
| `Dispatcher::active_count()` | Returns current number of held permits (lock-free, for observability) |
| `Dispatcher::max_concurrent()` | Returns the configured limit (0 = unlimited) |


### Git operations

Yoke includes a `git` module (`src/git.rs`) that provides repository management for the dispatcher pipeline. All git operations use the `git2` crate (libgit2 bindings).

| Function | Purpose |
|---|---|
| `sanitize_branch_name(branch)` | Collapse whitespace to `-`, strip non-`[a-zA-Z0-9._-]` characters |
| `build_clone_url(owner, repo, platform, token)` | Construct HTTPS clone URL with inline or header-based token auth |
| `GitAuth` | Credential callback struct for GitHub (`x-access-token`) and GitLab (`PRIVATE-TOKEN`) |
| `clone_repo(url, path, auth)` | Clone a remote repository with token authentication |
| `pull_repo(repo, branch, auth)` | Fetch + fast-forward merge for a branch |
| `create_worktree(repo, branch_name, worktree_path)` | Create a worktree (new branch from HEAD or existing branch) |
| `remove_worktree(repo, worktree_name)` | Remove a worktree by its administrative name |
| `has_uncommitted_changes(repo)` | Check for staged, unstaged, or untracked changes |

Token-based authentication: `GITHUB_TOKEN` or `GITLAB_TOKEN` environment variables are read at runtime and wired into the `git2` credential callbacks. GitHub tokens are passed via the `x-access-token` header; GitLab tokens use the `PRIVATE-TOKEN` header. Branch names are sanitized to `[a-zA-Z0-9._-]` before use in worktree creation.


### Workflow Runner

The `WorkflowRunner` (`src/runner.rs`) orchestrates sequential execution of multi-step workflows:

1. For each `Step` in the workflow, it runs **pre-hooks**, renders the `prompt_template` with template variables, calls the **Hermes API** via `HermesClient`, and runs **post-hooks**.
2. **Fail-fast**: the first error stops the entire workflow immediately.
3. Pre-hook failure prevents step execution; post-hook failure marks the step as failed.
4. Template variables (`{{key}}`) are substituted using the `template` module — unknown variables cause an error.

```rust
use yoke::runner::WorkflowRunner;
use yoke::harness::HermesClient;

let client = HermesClient::new("http://localhost:8000".into(), "api-key".into());
let runner = WorkflowRunner::new(workflow, variables, workspace_dir, client);
runner.run().await?; // Returns Ok(()) or Err(RunnerError)
```

#### Error types

`RunnerError` covers all failure modes:

| Variant | Cause |
|---|---|
| `Template` | Unknown variable, malformed syntax, or empty template |
| `Hook` | File not found, empty, or missing expected text |
| `Harness` | Network error, non-2xx API response, or IO error |
| `Execution` | Wrapping error from a failed step (includes step name) |

### Hot-reload

Yoke watches the `--workflows` directory for `.toml` file changes and emits reload events when files are created, modified, or deleted. This enables hot-reload of workflow definitions without restarting the server.

Key behaviors:

- **File filtering**: Only `.toml` files are monitored; other file types (`.txt`, `.md`, etc.) are ignored.
- **Debouncing**: Rapid successive changes within 500ms are collapsed into a single reload event, preventing unnecessary reload storms during multi-file edits or atomic save operations.
- **Detection latency**: File changes are detected within ~1 second (500ms debounce window + event processing).

| Message | Trigger |
|---|---|
| `FileChanged { path }` | A `.toml` file was created or modified |
| `FileRemoved { path }` | A `.toml` file was deleted |

The file watcher starts at application startup. If the watcher fails to initialize (e.g., the directory does not exist), a warning is logged and the server continues without hot-reload. The watcher runs in a background task; dropping the `FileWatcher` handle stops the watcher.

> **Note**: The actual workflow re-loading and state replacement is planned for a follow-up issue. Currently, the watcher logs detected changes but does not yet swap the live workflow set.

## Testing

### Unit tests

Unit tests live alongside the source code in `src/` using `#[cfg(test)]` modules. They exercise individual functions and methods in isolation.

### Integration tests

Integration tests live in `tests/` and exercise cross-cutting behavior through the public API (`yoke::dispatcher`, etc.). The `tests/dispatcher_tests.rs` file covers:

- Full dispatch flow (send → receive → complete callback)
- Duplicate event rejection across all dedup sets
- Concurrency limiting via `max_concurrent` semaphore
- Active count observability
- Graceful shutdown and drain of in-flight events
- Persistence of completed and failed events to disk
- GitLab and GitHub event dispatch
- Multiple distinct events processed independently

Running integration tests requires the library target (`src/lib.rs`), which re-exports all modules as `pub`.

## License

TBD
