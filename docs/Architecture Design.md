# Agent Orchestrator — Architecture Design

## 1. Overview

The orchestrator is a Rust daemon that receives webhook events from a code platform (GitHub or GitLab) via a built-in HTTP server, deduplicates them, and runs multi-step agent workflows through the Hermes Agent REST API.

A single orchestrator instance handles one platform. To support both GitHub and GitLab, run two instances with separate configs.

The code platform delivers webhook events to the orchestrator's HTTP server. The daemon verifies, parses, and routes each event to a workflow runner, which executes a sequence of agent steps via the Hermes `/v1/responses` endpoint. Each step is a prompt template rendered with event variables and sent as a request to the Hermes API.

### Design Goals

1. **Webhook-driven** — Platform webhooks as event sources. The daemon listens for events; it does not query for them.
2. **Hermes API** — All agent invocations go through the `/v1/responses` endpoint.
3. **Single platform per instance** — GitHub or GitLab, configured globally. Reduces complexity across config, routing, dedup, and authentication.
4. **Fail-fast with audit trail** — Startup errors are hard exits. Runtime errors are per-event soft failures. Every step is logged to disk.
5. **Graceful shutdown** — SIGINT/SIGTERM drains active workflows, persists state, exits. Second signal forces immediate exit.
6. **Hot-reload** — Workflow TOML files are reloaded on change without restart.
7. **Config separation** — User-specific settings (repos, agent instances, concurrency) live in a global `config.toml`. Workflow definitions (triggers, steps, git opts) are reusable `.toml` files that reference agents by name.

### Non-Goals

- Horizontal scaling or multi-instance coordination
- Built-in retry with exponential backoff (GitHub and GitLab handle webhook retries)
- Metrics / Prometheus endpoint
- Intra-workflow branching or conditional step execution
- Webhook delivery to external systems (the orchestrator receives webhooks, it doesn't send them)

## 2. High-Level Architecture

```
           ┌─────────────────────────────┐
           │   Code Platform             │
           │   (GitHub or GitLab)        │
           │   Webhooks UI              │
           └────────────┬────────────────┘
                        │ POST /webhook
                        ▼
           ┌──────────────────────────┐
           │     HTTP Server (axum)    │
           │  ┌─────────────────────┐  │
           │  │  Webhook Handler    │  │
           │  │  - HMAC/token auth  │  │
           │  │  - Parse payload    │  │
           │  │  - Quick dedup skip │  │
           │  │  - Build EventKey   │  │
           │  └─────────┬───────────┘  │
           └────────────┼──────────────┘
                        │ mpsc channel
                        ▼
           ┌──────────────────────────┐
           │       Dispatcher          │
           │  - Dedup (in_flight,      │
           │    completed, failed)     │
           │  - Semaphore-gated        │
           │  - Spawns tokio tasks     │
           └────────────┬──────────────┘
                        │ per event
                        ▼
           ┌──────────────────────────┐
           │      Workflow Runner       │
           │  - Git clone/worktree      │
           │  - Step loop               │
           │    pre-hooks → harness →   │
           │    post-hooks              │
           │  - Worktree cleanup         │
           └────────────┬──────────────┘
                        │ each step
                        ▼
           ┌──────────────────────────┐
           │   Hermes API Harness       │
           │   POST /v1/responses       │
           │   - instructions + input   │
           │   - Bearer auth            │
           │   - store: true            │
           └──────────────────────────┘
```

The architecture is a three-layer split: event ingestion (HTTP server with platform handler) → dispatch (dedup + concurrency) → workflow execution (steps + harness).

### Dedup Responsibility

The webhook handler does a lightweight check against the `completed` set to avoid enqueueing events that are already done — this is an optimization, not the authoritative dedup. The dispatcher is the source of truth: it checks all three sets (completed, permanently_failed, in_flight) and makes the final decision on whether to run a workflow.

### Repo Routing

The webhook handler identifies the repo the event belongs to and matches it against the `repos` list in `config.toml`. If no repo matches, the handler returns `200` (no-op).

- **GitHub**: extracts `repository.owner.login` and `repository.name` from the payload.
- **GitLab**: extracts `project.path_with_namespace` from the payload (maps directly to `owner/repo`).

## 3. Configuration Layout

Configuration is split into two files with distinct responsibilities:

1. **`config.toml`** — global, user-specific settings that tie the daemon to a particular deployment. Contains the platform choice, repos, named agent instances, runtime settings, and server settings.
2. **Workflow `.toml` files** — reusable workflow definitions that can be shared across deployments. Contain triggers, steps, git options, and a reference to an agent by name.

This separation means a workflow definition (e.g., "plan-then-implement") can be applied to any repo and any agent instance by wiring it in `config.toml`, without duplicating the step templates or prompt logic.

### config.toml

```toml
# Platform: "github" or "gitlab" — determines webhook handler, auth, and event types
platform = "github"

# Repos to monitor — shared across all workflows
repos = [
    { owner = "mintybasil", repo = "agent-orchestrator" },
    { owner = "mintybasil", repo = "enginedj-overlay" },
]

# Named agent instances (Hermes API configs)
[[agents]]
name = "local"
base_url = "http://localhost:8000"

[[agents]]
name = "remote"
base_url = "https://hermes.mycompany.com"

# Runtime settings
[runtime]
max_concurrent = 2                     # max concurrent workflows (0 = unlimited)
workdir = "~/.agent-orchestrator"       # runtime data directory

# Server settings
[server]
host = "0.0.0.0"
port = 8644
webhook_secret = "your-webhook-secret"  # GitHub HMAC key or GitLab token
max_body_size = 1048576                # 1MB default

# GitLab-specific (only when platform = "gitlab")
# gitlab_url = "https://gitlab.mycompany.com"  # for self-hosted GitLab
```

### Workflow Files

Workflow files live in the `--workflows` directory (default: `.`). Each file is a self-contained workflow definition:

```toml
# What events trigger this workflow
[trigger]
type = "github_issue_assigned"
assigned_to = "zeroklaw"

# Git configuration
[git]
clone = true
worktree = true

# Steps to execute (in order)
[[steps]]
name = "Plan"
agent = "local"
prompt_template = """
You are an expert software engineer. Issue {{owner}}/{{repo}}#{{issue_number}} has been assigned to you.
Read the issue and create an implementation plan.
Save the plan to {{output_path}}/plan.md

Issue details: {{{issue_body}}}
"""

[[steps]]
name = "Implement"
agent = "local"
prompt_template = """
You are an expert software engineer working on {{owner}}/{{repo}}#{{issue_number}}.
Read the plan at {{output_path}}/plan.md and implement it.
Create a PR with your changes.
"""
```

Each step specifies which agent to use via the `agent` field — a string reference to an entry in `config.toml`'s `[[agents]]` array. At startup, the orchestrator resolves every step's `agent` name to the agent's `base_url`. If any step references an agent name that doesn't match a configured agent, startup fails with a hard exit.

### Trigger Type Naming

Trigger types are prefixed with the platform name — `github_` or `gitlab_` — matching the event semantics of that platform. A workflow with a trigger type that doesn't match the configured platform is rejected at startup with a hard exit.

**GitHub trigger types:**

| Type                                 | GitHub Event                  | Action      |
| ------------------------------------ | ----------------------------- | ----------- |
| `github_issue_assigned`              | `issues`                      | `assigned`  |
| `github_issue_comment`               | `issue_comment`               | `created`   |
| `github_pull_request_review`         | `pull_request_review`         | `submitted` |
| `github_pull_request_review_comment` | `pull_request_review_comment` | `created`   |

**GitLab trigger types:**

| Type                                  | GitLab Event | Action/Object Kind                                     |
| ------------------------------------- | ------------ | ------------------------------------------------------ |
| `gitlab_issue_assigned`               | `Issue Hook` | `issue` (action: `update`)                             |
| `gitlab_note`                         | `Note Hook`  | `note`                                                 |
| `gitlab_merge_request_review`         | `Note Hook`  | `note` (noteable_type = MergeRequest)                  |
| `gitlab_merge_request_review_comment` | `Note Hook`  | `note` (noteable_type = MergeRequest, type = DiffNote) |

The mapping from trigger type to platform event headers and payload fields is handled internally by the webhook handler.

### How Repos Connect to Workflows

All repos listed in `config.toml` share the same set of loaded workflows. When a webhook arrives for a repo, the dispatcher finds all workflows whose `[trigger]` matches the event, then runs them. This means a single workflow file automatically applies to every configured repo.

If different repos need different workflows, use `[trigger]` filters (e.g., `assigned_to`, `allowed_users`) to scope which events each workflow responds to.

### Field Reference

**config.toml fields:**

| Field | Purpose | Default |
|---|---|---|
| `platform` | `"github"` or `"gitlab"` | required |
| `repos` | Array of `{owner, repo}` entries | required |
| `repos[].owner` | Repository owner / namespace | required |
| `repos[].repo` | Repository name | required |
| `gitlab_url` | Self-hosted GitLab base URL (GitLab only) | `https://gitlab.com` |
| `[[agents]]` | Named Hermes API instances | required (at least one) |
| `agents[].name` | Unique name for referencing in workflows | required |
| `agents[].base_url` | Hermes API host (no path) | required |
| `[runtime].max_concurrent` | Max concurrent workflow runs | `0` (unlimited) |
| `[runtime].workdir` | Runtime data directory | `~/.agent-orchestrator` |
| `[server].host` | Bind address | `0.0.0.0` |
| `[server].port` | Listen port | `8644` |
| `[server].webhook_secret` | Webhook auth key (HMAC for GitHub, token for GitLab) | required |
| `[server].max_body_size` | Request body limit (bytes) | `1048576` |

**Workflow file fields:**

| Field | Purpose | Default |
|---|---|---|
| `[trigger].type` | Event type (e.g. `github_issue_assigned`, `gitlab_merge_request_review`) | required |
| `[trigger].assigned_to` | Filter: only fire for this assignee | none (any) |
| `[trigger].allowed_users` | Filter: only fire for these users | none (any) |
| `[git].clone` | Whether to git clone the repo | `true` |
| `[git].worktree` | Whether to create a per-event worktree | `true` |
| `[git].default_branch` | Branch for clone/worktree base | `"main"` |
| `[[steps]].name` | Human-readable step label | required |
| `[[steps]].agent` | Name of agent from `config.toml` | required |
| `[[steps]].prompt_template` | `{{variable}}` template | required |
| `[[steps]].pre_hooks` | Hooks to check before step | none |
| `[[steps]].post_hooks` | Hooks to check after step | none |

## 4. Event Sources (Webhooks)

The orchestrator runs a single webhook handler, determined by the `platform` setting in `config.toml`. The handler is registered at `POST /webhook`.

### GitHub Webhooks

When `platform = "github"`, the handler at `POST /webhook` receives GitHub webhook deliveries. Each delivery includes:

- `X-GitHub-Event` header — the event type (`issues`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`)
- `X-Hub-Signature-256` header — HMAC-SHA256 signature for verification
- `webhook_secret` is the HMAC-SHA256 key
- JSON payload — the event data

### GitLab Webhooks

When `platform = "gitlab"`, the handler at `POST /webhook` receives GitLab webhook deliveries. Each delivery includes:

- `X-GitLab-Event` header — the event type (`Issue Hook`, `Note Hook`)
- `X-Gitlab-Token` header — static token for verification
- `webhook_secret` is the token value compared against this header
- JSON payload — the event data

### Verification

| Platform | Header | Mechanism |
|---|---|---|
| GitHub | `X-Hub-Signature-256` | HMAC-SHA256 of the request body with `webhook_secret` |
| GitLab | `X-Gitlab-Token` | Constant-time comparison of the header value against `webhook_secret` |

Unverified payloads receive a `401` response and are logged as a warning. This prevents forgery and ensures the daemon only processes legitimate events.

### Event Mapping

The handler maps platform-native webhook events to internal `TriggerEvent` types based on workflow configuration. Trigger types carry the platform prefix, so the handler matches them directly against the platform's event headers and payload fields.

**GitHub mapping:**

| Trigger Type | GitHub Event | Action | Variables |
|---|---|---|---|
| `github_issue_assigned` | `issues` | `assigned` | `issue_number`, `action`, `assignee`, `issue_title`, `issue_body` |
| `github_issue_comment` | `issue_comment` | `created` | `issue_number`, `comment_id` |
| `github_pull_request_review` | `pull_request_review` | `submitted` | `pr_number`, `review_id`, `review_body` |
| `github_pull_request_review_comment` | `pull_request_review_comment` | `created` | `pr_number`, `review_id`, `comment_id` |

**GitLab mapping:**

| Trigger Type                          | GitLab Event | Object Kind                                            | Variables                                                               |
| ------------------------------------- | ------------ | ------------------------------------------------------ | ----------------------------------------------------------------------- |
| `gitlab_issue_assigned`               | `Issue Hook` | `issue`                                                | `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body` |
| `gitlab_note`                         | `Note Hook`  | `note`                                                 | `issue_iid` or `mr_iid`, `note_id`                                      |
| `gitlab_merge_request_review`         | `Note Hook`  | `note` (noteable_type = MergeRequest)                  | `mr_iid`, `review_id`, `review_body`                                    |
| `gitlab_merge_request_review_comment` | `Note Hook`  | `note` (noteable_type = MergeRequest, type = DiffNote) | `mr_iid`, `review_id`, `comment_id`                                     |

The mapping is configured per-workflow via the `[trigger]` TOML section. Triggers specify which **event + action combinations** to respond to, along with optional filters (e.g., only fire when a specific user is assigned, or when a review is submitted by specific users).

### Webhook Reliability

Both platforms retry webhook deliveries if the endpoint doesn't return 2xx:

**GitHub**: Retries up to 3 times with increasing delays (roughly 5s, 15s, 45s). Provides resilience against brief restarts and momentary overload.

**GitLab**: Retries up to 4 times with exponential backoff (up to ~50s between attempts for self-hosted; GitLab.com uses similar logic). Provides equivalent resilience.

For longer outages, the platform marks the delivery as failed and stops retrying. The user would need to check the platform's webhook delivery logs to identify missed events. A future enhancement could add a "catch-up" mode that queries the platform's API for recent events since the last known delivery, but this is out of scope for the initial release.

## 5. HTTP Server

### Stack

- **axum** as the HTTP framework (lightweight, tokio-native, good ecosystem)
- **tower** middleware for logging, CORS (if needed), and request body limits

### Endpoints

| Method | Path | Purpose |
|---|---|---|
| POST | `/webhook` | Receive platform webhook deliveries |
| GET | `/health` | Health check (returns `{"status": "ok"}`) |
| GET | `/ready` | Readiness check (returns 200 when dispatcher is accepting events) |

### Request Flow

1. Platform sends `POST /webhook` with event payload
2. Tower middleware logs the request and enforces body size limit (1MB default)
3. Handler extracts platform-specific headers and verifies authenticity
   - GitHub: extracts `X-GitHub-Event` + `X-Hub-Signature-256`, verifies HMAC-SHA256
   - GitLab: extracts `X-GitLab-Event` + `X-Gitlab-Token`, verifies token
4. If verification fails, returns `401`
5. Handler parses the JSON payload into a structured event
6. Handler checks if the event type + action matches any configured trigger
7. If no trigger matches, returns `200` (no-op — platform doesn't need to retry)
8. If a trigger matches, handler builds a `TriggerEvent` and sends it through the mpsc channel
9. Returns `200` immediately — the handler never blocks on workflow execution

## 6. Dispatcher

The dispatcher consumes `DispatchMessage`s from the mpsc channel, manages dedup sets (in_flight, completed, permanently_failed), and throttles concurrency via a tokio semaphore. It is a pure consumer — it processes events queued by the webhook handler.

### Dedup Logic

- `{owner}/{repo}/{workspace_id}` as the dedup key
- Completed events are skipped
- In-flight events are skipped
- Permanently-failed events are skipped
- `[runtime].max_concurrent` from `config.toml` sets the semaphore capacity. 0 = unlimited.
- `completed.json` and `failed.json` with atomic writes for persistence.

### Concurrency Model

```
┌──────────────────┐  ┌──────────────────┐
│  Webhook Handler  │  │  Signal Handler   │
│  (axum route)     │  │  (SIGINT/SIGTERM) │
└────────┬──────────┘  └────────┬──────────┘
         │ mpsc                 │ watch
         ▼                      ▼
┌────────────────────────┐  ┌──────────────────────┐
│      Dispatcher         │  │  (shutdown signal)   │
│  (single consumer)     │  └──────────────────────┘
│  - dedup check         │
│  - semaphore acquire   │
│  - spawn workflow task │
│  - track in_flight     │
│  - drain on shutdown   │
└────────────────────────┘
         │ per event (tokio::spawn)
         ▼
┌────────────────────────┐
│  Workflow Runner (N)  │
│  - Git ops            │
│  - Step execution     │
│  - Hermes API call    │
│  - Cleanup            │
└────────────────────────┘
```

The dispatcher loop runs as a single tokio task, so the dedup check + in_flight insert is sequential (no races). Workflow runners are spawned as independent tokio tasks.

## 7. Workflow Engine

### Step Structure

Each step has:
- `name` — human-readable label (used in log file names)
- `agent` — name of the Hermes API instance to use (references `[[agents]]` in `config.toml`)
- `prompt_template` — `{{variable}}` template rendered with event + global variables
- `pre_hooks` — optional list of hooks to check before running the step
- `post_hooks` — optional list of hooks to check after running the step

The `agent` field on each step allows different steps in the same workflow to target different Hermes API instances.

### Template Variables

Global variables provided by the runner:

| Variable | Value |
|---|---|
| `owner` | Repository owner (namespace) |
| `repo` | Repository name |
| `default_branch` | From `[git].default_branch` |
| `output_path` | Per-event workspace directory |
| `repo_path` | Path to the repo clone (empty if `git.clone = false`) |

Trigger-specific variables:

**GitHub:**

| Trigger Type                         | Variables                                                         |
| ------------------------------------ | ----------------------------------------------------------------- |
| `github_issue_assigned`              | `issue_number`, `action`, `assignee`, `issue_title`, `issue_body` |
| `github_issue_comment`               | `issue_number`, `comment_id`                                      |
| `github_pull_request_review`         | `pr_number`, `review_id`, `review_body`                           |
| `github_pull_request_review_comment` | `pr_number`, `review_id`, `comment_id`                            |

**GitLab:**

| Trigger Type                          | Variables                                                               |
| ------------------------------------- | ----------------------------------------------------------------------- |
| `gitlab_issue_assigned`               | `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body` |
| `gitlab_note`                         | `issue_iid` or `mr_iid`, `note_id`                                      |
| `gitlab_merge_request_review`         | `mr_iid`, `review_id`, `review_body`                                    |
| `gitlab_merge_request_review_comment` | `mr_iid`, `review_id`, `comment_id`                                     |

Additional variables are extracted automatically from the platform's event JSON and merged into the template variable map. Webhook payloads carry the full event data, giving template authors access to rich context beyond just numeric IDs.

### Prompt Template Validation

At startup, the orchestrator validates all prompt templates:

- **Variable existence**: Each `{{variable}}` placeholder is checked against the known set of global and trigger-specific variables. Unknown variables cause a hard exit.
- **Syntax errors**: Malformed placeholders (e.g., `{{variable`, `{{ }}`) are rejected.
- **Empty templates**: Templates that are empty or whitespace-only after rendering are flagged.

This catches user error early, before any webhook is received.

### Hooks

Pre/post step hooks:

| Hook | Checks |
|---|---|
| `file_not_empty` | A file has non-zero content |
| `file_contains` | A file contains a specific string |

Hooks are configured per-step.

### Dedup & Persistence

- **completed.json** — set of `{owner}/{repo}/{workspace_id}` strings for events that completed successfully
- **failed.json** — array of `{key, timestamp, error}` entries for events that failed
- Atomic file writes (write to `.tmp`, rename)
- Loaded on startup, appended to on completion/failure

The dedup key format: `{owner}/{repo}/{workspace_id}`. For issue events, `workspace_id` is the issue number (GitHub) or IID (GitLab). For PR/MR reviews, it's `{pr_number}_review-{review_id}` (GitHub) or `{mr_iid}_review-{note_id}` (GitLab).

## 8. Agents (Hermes API Harness)

Agents are named Hermes API instances defined in `config.toml`. Workflow files reference an agent by name, keeping deployment-specific connection details out of reusable workflow definitions.

### Agent Configuration (config.toml)

```toml
[[agents]]
name = "local"
base_url = "http://localhost:8000"  # Host-only (e.g. http://localhost:8000)
```

### Request Format

When the harness executes a step, it builds a request to the agent's `base_url`:

```json
{
  "instructions": "All work is in: /path/to/workspace. Always run `cd /path/to/workspace` as your first action. Platform: github",
  "input": "<rendered prompt>",
  "store": true
}
```

### Agent Resolution

At startup, every step's `agent` field is resolved against the `[[agents]]` array in `config.toml`. If any step references an agent name that doesn't match a configured agent, the orchestrator exits with a hard error. This ensures misconfigured workflows fail immediately.

### Conventions

- Uses `/v1/responses` endpoint
- `base_url` is host-only — the internal path `/v1/responses` is a constant in code
- Auth via `HERMES_API_KEY` env var (checked per invocation, never in config)
- `instructions` carries workspace path with explicit `cd` directive, plus the `platform` identifier
- Response parsing extracts `output[].content[].type == "output_text"` blocks
- `HarnessConfig` is a single struct (not an enum)
- Harness implements a `Harness` trait for extensibility

## 9. Git & Worktree Management

The orchestrator manages the git lifecycle for each configured repo:

1. **Clone** — `git clone` (via git2 crate) on first event, `git pull` on subsequent events
2. **Worktree** — optional per-event worktree using `git worktree add`
3. **Branch naming** — `ao/<sanitized-label>-<unix-timestamp>`
4. **Cleanup** — check for uncommitted changes, remove worktree, delete branch

Authentication via git2 `RemoteCallbacks` with token-based credentials. The token env var is determined by the `platform` setting:

| Platform | Env Var | Clone URL Pattern |
|---|---|---|
| GitHub | `GITHUB_TOKEN` | `https://x-access-token:{token}@github.com/{owner}/{repo}.git` |
| GitLab | `GITLAB_TOKEN` | `https://oauth2:{token}@{gitlab_host}/{owner}/{repo}.git` |

Where `gitlab_host` is `gitlab.com` by default, or the value of `gitlab_url` for self-hosted instances.

Token never embedded in URLs or git config stored persistently — only used in the clone/pull `RemoteCallbacks`.

The Hermes API agent receives the worktree path via the `instructions` field and uses `cd <path>` as its first action. If the path doesn't exist or isn't accessible, the agent falls back to the platform's file API via MCP tools (GitHub Contents API or GitLab Repository Files API).

### Self-Hosted GitLab

When `platform = "gitlab"`, the optional `gitlab_url` field overrides the default `https://gitlab.com`:

```toml
platform = "gitlab"
gitlab_url = "https://gitlab.mycompany.com"
repos = [
    { owner = "internal-team", repo = "backend-service" },
]
```

This affects clone URL construction and is useful for documentation and future API integration.

## 10. Concurrency Model

```
1 HTTP server task (axum)
1 Webhook handler route (platform-specific)
1 Dispatcher task (consumes channel, spawns workflows)
N Workflow runner tasks (capped by Semaphore)
1 Signal handler task (SIGINT/SIGTERM)
```

All managed by a single tokio runtime. Shared state via `Arc<Mutex<_>>` for the dedup sets. The webhook handler sends on a bounded mpsc channel (default capacity: 100) — if the dispatcher is overwhelmed, the handler returns `503 Service Unavailable` instead of blocking.

### Graceful Shutdown

1. First SIGINT/SIGTERM: signal handler sends `true` on the watch channel
2. HTTP server stops accepting new connections (but finishes in-flight requests)
3. Dispatcher stops consuming from the channel
4. Active workflow runners drain to completion (bounded by a configurable timeout)
5. State is persisted (completed.json, failed.json updated)
6. Process exits
7. Second signal: immediate `process::exit(1)`

## 11. Data Directory Layout

```
{workdir}/
  completed.json              # Set of completed event keys
  failed.json                 # Array of failure entries
  {owner}/{repo}/
    repo/                     # git clone
    {workspace_id}/           # per-event workspace
      worktree-{N}/           # per-event worktree (if git.worktree = true)
      step_00_Plan.log        # Full Hermes API request + response, with final message rendered
      step_00_Plan.error      # Error details (if step failed)
      step_00_Plan.prompt     # Rendered prompt for auditing
      step_01_Implement.log
      step_01_Implement.error
      step_01_Implement.prompt
```

`step_XX_<name>.log` contains the full HTTP exchange: the request body sent to Hermes API, the response received, and the extracted final message (from `output[].content[].type == "output_text"`) rendered in a human-readable format at the end of the file.

## 12. Error Handling

Two-tier model: startup errors are hard exits, runtime errors are per-event soft failures.

### Startup Hard Exits

- Missing `config.toml` or invalid TOML
- Missing `platform` field
- Invalid `platform` value (must be `"github"` or `"gitlab"`)
- Unknown agent name in a workflow file (doesn't match any `[[agents]]` entry)
- Missing platform token env var (`GITHUB_TOKEN` for github, `GITLAB_TOKEN` for gitlab)
- Missing `HERMES_API_KEY` env var
- Invalid `agents[].base_url` (contains a path segment like `/v1` or `/chat`)
- Missing `webhook_secret` in `[server]`
- Data directory not writable
- No workflow `.toml` files found
- Trigger type with wrong platform prefix (e.g., `gitlab_issue_assigned` when `platform = "github"`)

### Runtime Per-Event Soft Failures

- Verification failure (HMAC or token) → `401`, logged as warning, not a workflow failure
- Webhook payload parse failure → `400`, logged as warning
- No matching trigger → `200` (no-op)
- Workflow runner failure → event added to `permanently_failed`, error logged
- Hermes API non-2xx → error written to `.error` file, step fails
- Git clone/pull failure → workflow fails
- Worktree cleanup failure → logged, workflow result preserved

## 13. Module Map

| File | Responsibility |
|---|---|
| `src/main.rs` | Entry point: startup validation, tracing init, server + dispatcher + signal handler |
| `src/config.rs` | Config struct (TOML): config.toml + workflow files, clap CLI |
| `src/server.rs` | axum HTTP server: router, middleware, health endpoint |
| `src/webhook/mod.rs` | Webhook handler dispatch: selects GitHub or GitLab handler based on `platform` |
| `src/webhook/github.rs` | GitHub webhook handler: HMAC-SHA256 verify, payload parse, event mapping |
| `src/webhook/gitlab.rs` | GitLab webhook handler: token verify, payload parse, event mapping |
| `src/dispatcher.rs` | Concurrency control: dedup sets, semaphore, mpsc consumer, persistence |
| `src/runner.rs` | Per-event workflow execution: git ops, step loop, template rendering |
| `src/harness.rs` | Harness trait + single HermesApiHarness implementation |
| `src/git.rs` | Git repo/worktree management: clone/pull, worktree create/remove, auth |
| `src/hooks.rs` | Hook enum + run_hook() dispatcher |
| `src/template.rs` | `{{key}}` placeholder renderer |
| `src/workflow.rs` | Step type definition |
| `src/github.rs` | GitHub webhook payload types (event structs) |
| `src/gitlab.rs` | GitLab webhook payload types (event structs) |

## 14. CLI

```bash
agent-orchestrator [OPTIONS]

Options:
  --config <FILE>              Path to config.toml (default: ./config.toml)
  --workflows <DIR>            Directory containing workflow TOML files (default: .)
  --show-logs                  Print harness output to terminal
  --host <ADDR>                Server bind address (overrides config.toml)
  --port <PORT>                Server listen port (overrides config.toml)
```

`--host` and `--port` override `config.toml` values. `[runtime].max_concurrent`, `[runtime].workdir`, and `platform` are set in `config.toml` (no CLI flags).

## 15. Environment Variables

| Variable | Purpose | Required |
|---|---|---|
| `GITHUB_TOKEN` | GitHub authentication for git clone/pull | When `platform = "github"` |
| `GITLAB_TOKEN` | GitLab authentication for git clone/pull | When `platform = "gitlab"` |
| `HERMES_API_KEY` | Bearer token for Hermes REST API | Yes |
| `WEBHOOK_SECRET` | Webhook auth key (overrides config.toml `webhook_secret`) | Yes |

## 16. Example Configs

### config.toml (GitHub)

```toml
platform = "github"

repos = [
    { owner = "mintybasil", repo = "agent-orchestrator" },
    { owner = "mintybasil", repo = "enginedj-overlay" },
]

[[agents]]
name = "local"
base_url = "http://localhost:8000"

[[agents]]
name = "remote"
base_url = "https://hermes.mycompany.com"

[runtime]
max_concurrent = 2
workdir = "~/.agent-orchestrator"

[server]
host = "0.0.0.0"
port = 8644
webhook_secret = "your-github-webhook-secret"
```

### config.toml (GitLab, self-hosted)

```toml
platform = "gitlab"
gitlab_url = "https://gitlab.mycompany.com"

repos = [
    { owner = "internal-team", repo = "backend-service" },
    { owner = "internal-team", repo = "frontend-app" },
]

[[agents]]
name = "local"
base_url = "http://localhost:8000"

[runtime]
max_concurrent = 2
workdir = "~/.agent-orchestrator"

[server]
host = "0.0.0.0"
port = 8644
webhook_secret = "your-gitlab-webhook-token"
```

### Workflow: GitHub issue plan+implement

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "zeroklaw"

[git]
clone = true
worktree = true

[[steps]]
name = "Plan"
agent = "local"
prompt_template = """
Plan the implementation for {{owner}}/{{repo}}#{{issue_number}}.
Save the plan to {{output_path}}/plan.md
"""

[[steps]]
name = "Implement"
agent = "local"
prompt_template = """
Read the plan at {{output_path}}/plan.md and implement it for {{owner}}/{{repo}}#{{issue_number}}.
Create a PR with your changes.
"""
```

### Workflow: GitHub PR review response

```toml
[trigger]
type = "github_pull_request_review"
allowed_users = ["zeroklaw"]

[git]
clone = true
worktree = true

[[steps]]
name = "Address Review"
agent = "remote"
prompt_template = """
Address the review feedback on {{owner}}/{{repo}}#{{pr_number}}.
Review ID: {{review_id}}
"""
```

### Workflow: GitLab issue plan+implement

```toml
[trigger]
type = "gitlab_issue_assigned"

[git]
clone = true
worktree = true

[[steps]]
name = "Plan"
agent = "local"
prompt_template = """
Plan the implementation for {{owner}}/{{repo}}#!{{issue_iid}}.
Save the plan to {{output_path}}/plan.md
"""

[[steps]]
name = "Implement"
agent = "local"
prompt_template = """
Read the plan at {{output_path}}/plan.md and implement it for {{owner}}/{{repo}}#!{{issue_iid}}.
Create an MR with your changes.
"""
```

### Workflow: GitLab merge request review response

```toml
[trigger]
type = "gitlab_merge_request_review"

[git]
clone = true
worktree = true

[[steps]]
name = "Address Review"
agent = "remote"
prompt_template = """
Address the review feedback on {{owner}}/{{repo}}#!{{mr_iid}}.
Review ID: {{review_id}}
"""
```

## 17. Design Decisions (Resolved)

1. **Single platform per instance**: The orchestrator handles one platform (GitHub or GitLab) per instance, set globally in `config.toml`. Supporting both in a single instance adds complexity across config, routing, dedup, authentication, and data layout for a marginal use case. Running two instances with separate configs is simpler to operate and reason about.

2. **Unified webhook path**: A single `POST /webhook` endpoint — only one handler is active at a time, selected by the `platform` setting. There is no ambiguity about which verification and parsing logic to apply.

3. **Platform-specific trigger types**: Trigger types carry the platform prefix (e.g., `github_issue_assigned`, `gitlab_merge_request_review`). GitHub and GitLab have different event models, payload shapes, and action semantics — unified names paper over real differences and create ambiguous mappings. Prefixed types make workflows explicit about which platform they target. At startup, any workflow containing a trigger type that doesn't match the configured platform is rejected with a hard exit, catching misconfigured workflows immediately.

4. **Payload size limit**: Configurable via `[server].max_body_size`. Default 1MB. GitHub and GitLab payloads are typically <100KB, but large diffs can exceed that. Users who hit the limit can increase it.

5. **HTTPS/TLS**: Reverse proxy is the expected pattern. The HTTP server listens on plain HTTP. For production, put it behind Caddy, nginx, or a cloudflare tunnel. TLS termination is not the orchestrator's job — it's infrastructure.

6. **Webhook secret rotation**: Restart required. Changing the secret in the platform's webhook settings and then restarting the orchestrator is a simple, reliable workflow. Hot-reloading secrets adds complexity (race conditions between the old and new secret during rotation) for marginal benefit.

7. **Multi-workflow dedup**: Shared dedup sets. If two workflows match the same event (e.g., both `github_issue_assigned` for overlapping repos), the first workflow loaded runs. Per-workflow dedup would require tracking completed events per-workflow-file, which doubles the persistence complexity for a marginal use case. If this becomes a problem, the user should scope triggers more tightly.

8. **GitLab token verification**: GitLab webhook verification uses a static token in the `X-Gitlab-Token` header, not HMAC. This is GitLab's standard mechanism. The comparison is done in constant time to prevent timing attacks.

9. **Config separation**: Global user settings (`platform`, `repos`, `agents`, `[runtime]`, `[server]`) live in `config.toml`. Workflow definitions (`[trigger]`, `[git]`, `[[steps]]`) live in separate `.toml` files. Each step carries its own `agent` reference, keeping deployment-specific connection details out of workflow definitions while allowing steps to target different agents.

10. **Named agents**: `[[agents]]` in `config.toml` defines named Hermes API instances. Each step in a workflow references an agent by name (`agent = "local"`), keeping `base_url` out of workflow files and making it easy to retarget a step by changing the config.

11. **Shared repos**: All repos in `config.toml` share the same workflow files. This simplifies the mental model — adding a new repo means one entry in the `repos` array, and every existing workflow automatically applies. Trigger filters (`assigned_to`, `allowed_users`) scope which events each workflow responds to.

12. **Repos as a flat array**: `repos = [{owner, repo}]` instead of `[[repos]]` (TOML array-of-tables). Repos are small, uniform entries — a flat array reads more compactly and avoids the visual noise of `[[repos]]` repeated headers for a two-field struct.

13. **Step-level agent assignment**: Each step declares its own `agent` field rather than a single workflow-level agent. This allows a workflow to use different Hermes API instances for different steps (e.g., a planning step on a local agent, an implementation step on a remote agent).

14. **Hermes-only agent config**: The `[[agents]]` config contains only `name` and `base_url`. Provider and model selection are Hermes Agent internals — the orchestrator sends `instructions` and `input` to `/v1/responses`, and Hermes handles provider routing and model selection. Exposing `provider` or `model` in the orchestrator config would leak Hermes internals and create a maintenance burden whenever the Hermes API changes.

15. **Assignment-only issue triggers**: The `github_issue_assigned` and `gitlab_issue_assigned` trigger types fire on the assignment event only, not on issue open. "Issue opened" is a semantically distinct event (the issue exists but no one is responsible for acting on it yet) and warrants its own trigger type if needed in the future. Conflating the two would require workflows to handle two different contexts (newly filed vs. explicitly assigned) in the same template logic.

16. **GitLab review triggers mirror GitHub**: GitLab does not have separate webhook events for "review submitted" vs. "inline review comment" — both arrive as `Note Hook` events. The orchestrator splits them into `gitlab_merge_request_review` (any Note on a MergeRequest) and `gitlab_merge_request_review_comment` (DiffNote on a MergeRequest) to maintain naming parity with GitHub's `github_pull_request_review` and `github_pull_request_review_comment`. The split is implemented by inspecting the `noteable_type` and `type` fields in the Note Hook payload. This gives workflow authors a consistent trigger vocabulary across platforms, even though the underlying webhook mechanism differs.
