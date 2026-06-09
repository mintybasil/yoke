# Yoke — Architecture Design

## 1. Overview

Yoke is a Rust daemon that receives webhook events from a code platform (GitHub or GitLab) via a built-in HTTP server and runs multi-step agent workflows through the Hermes Agent REST API.

The code platform delivers webhook events to Yoke's HTTP server. The daemon verifies, parses, and routes each event to a workflow runner, which executes a sequence of agent steps. Each step is a prompt template rendered with event variables and sent as a request to the Hermes API.

### Design Goals

1. **Webhook-driven** — Platform webhooks as event sources. The daemon listens for events; it does not query for them.
2. **Hermes API** — All agent invocations go through the `/v1/responses` endpoint.
3. **Single platform per instance** — GitHub or GitLab, configured globally. Reduces complexity across config, routing, dedup, and authentication.
4. **Fail-fast with audit trail** — Startup errors are hard exits. Runtime errors are per-event soft failures. Every step is logged to disk.
5. **Graceful shutdown** — SIGINT/SIGTERM drains active workflows, persists state, exits. Second signal forces immediate exit.
6. **Hot-reload** — Workflow TOML files are reloaded on change without restart.
7. **Config separation** — User-specific settings (repos, agent instances, concurrency) live in a global `config.toml`. Workflow definitions (triggers, steps, git opts) are reusable `.toml` files that reference agents by name.

## 2. High-Level Architecture

```
           ┌─────────────────────────────┐
           │   Code Platform             │
           │   (GitHub or GitLab)        │
           │   Webhooks UI               │
           └────────────┬────────────────┘
                        │ POST /webhook
                        ▼
           ┌──────────────────────────┐
           │     HTTP Server (axum)   │
           │  ┌────────────────────┐  │
           │  │  Webhook Handler   │  │
           │  │  - HMAC/token auth │  │
           │  │  - Parse payload   │  │
           │  │  - Quick dedup skip│  │
           │  │  - Build EventKey  │  │
           │  └────────┬───────────┘  │
           └───────────┼──────────────┘
                       │ mpsc channel
                       ▼
           ┌──────────────────────────┐
           │       Dispatcher         │
           │  - Dedup (in_flight,     │
           │    completed, failed)    │
           │  - Semaphore-gated       │
           │  - Spawns tokio tasks    │
           └────────────┬─────────────┘
                        │ per event
                        ▼
           ┌──────────────────────────┐
           │      Workflow Runner     │
           │  - Git clone/shallow clone │
           │  - Step loop             │
           │    pre-hooks → harness → │
           │    post-hooks            │
           │  - Clone cleanup         │
           └────────────┬─────────────┘
                        │ each step
                        ▼
           ┌──────────────────────────┐
           │   Hermes API Harness     │
           │   POST /v1/responses     │
           │   - instructions + input │
           │   - Bearer auth          │
           │   - store: true          │
           └──────────────────────────┘
```

The architecture is a three-layer split: event ingestion (HTTP server with platform handler) → dispatch (dedup + concurrency) → workflow execution (steps + harness).

## 3. Configuration Layout

Configuration is split into two files with distinct responsibilities:

1. **`config.toml`** — global settings, including platform choice, repos, named agent instances, runtime settings, and server settings.
2. **Workflow `.toml` files** — reusable workflow definitions that can be shared across deployments. Contain triggers, steps, git options, and a reference to an agent by name.

This separation means a workflow definition can be applied to any repo and any agent instance by wiring it in `config.toml`, without duplicating the step templates or prompt logic.

### config.toml

```toml
# Platform: "github" or "gitlab" — determines webhook handler, auth, and event types
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
max_concurrent = 2                     # max concurrent workflows (0 = unlimited)
workdir = "~/.yoke"       # runtime data directory

# Server settings
[server]
host = "0.0.0.0"
port = 8644
webhook_host = "yoke.example.com"
max_body_size = 1048576                # 1MB default
catch_up_enabled = true                # replay missed events on startup
catch_up_max_age_hours = 24            # max age of events to replay

# GitLab-specific (only when platform = "gitlab")
# gitlab_url = "https://gitlab.mycompany.com"  # for self-hosted GitLab
```

### Workflow Files

Workflow files live in the `--workflows` directory (default: `./workflows`). Each file is a self-contained workflow definition:

```toml
# What events trigger this workflow
[trigger]
type = "github_issue_assigned"
assigned_to = "alice"

# Git configuration
[git]
clone = true
shallow_clone = true

# Steps to execute (in order)
[[steps]]
name = "Plan"
agent = "pm"
prompt_template = """
You are an expert software engineer. Issue {{owner}}/{{repo}}#{{issue_number}} has been assigned to you.
Read the issue and create an implementation plan.
Save the plan to {{output_dir}}/plan.md

Issue details: {{{issue_body}}}
"""

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = """
You are an expert software engineer working on {{owner}}/{{repo}}#{{issue_number}}.
Read the plan at {{output_dir}}/plan.md and implement it.
Create a PR with your changes.
"""
```

Each step specifies which agent to use via the `agent` field — a string reference to an entry in `config.toml`'s `[[agents]]` array. At startup, Yoke resolves every step's `agent` name to the agent's `base_url`. If any step references an agent name that doesn't match a configured agent, startup fails with a hard exit.

### Prompt Template Variables

These variables are available in all prompt templates, regardless of trigger type:

| Variable         | Value                              |
|------------------|------------------------------------|
| `owner`          | Repository owner (namespace)       |
| `repo`           | Repository name                    |
| `output_dir`     | Per-event workspace directory      |
| `event_id`       | Canonical event identifier (e.g. `issue-42`, `pr-7-review-999`) — defined per trigger type in Appendix A |
| `repo_path`      | Full repository path (`owner/repo`) |

Additional trigger-specific variables are also available. See Appendix A for details.

### How Repos Connect to Workflows

All repos listed in `config.toml` share the same set of loaded workflows. When a webhook arrives for a repo, the dispatcher finds all workflows whose `[trigger]` matches the event, then runs them. This means a single workflow file automatically applies to every configured repo.

### Trigger Authorization

`allowed_users` exists to prevent prompt injection attacks by restricting which users can invoke a workflow. Without this check, any user who can create a webhook event (by assigning an issue, writing a comment, or submitting a review) could trigger arbitrary agent workflows — including workflows that have access to production repositories and infrastructure.

The **actor** is the user who **performed the action** that created the webhook event. This is the person who assigned the issue, wrote the comment, or submitted the review.

The actor must be extracted from the webhook payload's `sender` field (GitHub) or equivalent (GitLab) at webhook receipt time.

See **Appendix A** for the actor source mapping per trigger type.

### Field Reference

**config.toml fields:**

| Field                      | Purpose                                              | Default                 |
|----------------------------|------------------------------------------------------|-------------------------|
| `platform`                 | `"github"` or `"gitlab"`                             | required                |
| `repos`                    | Array of `{owner, repo}` entries                     | required                |
| `repos[].owner`            | Repository owner / namespace                         | required                |
| `repos[].repo`             | Repository name                                      | required                |
| `gitlab_url`               | Self-hosted GitLab base URL (GitLab only)            | `https://gitlab.com`    |
| `[[agents]]`               | Named Hermes API instances                           | required (at least one) |
| `agents[].name`            | Unique name for referencing in workflows             | required                |
| `agents[].base_url`        | Hermes API host (no path)                            | required                |
| `[runtime].max_concurrent` | Max concurrent workflow runs                         | `0` (unlimited)         |
| `[runtime].workdir`        | Runtime data directory                               | `~/.yoke`               |
| `[server].host`                     | Bind address                                         | `0.0.0.0`               |
| `[server].port`                     | Listen port                                          | `8644`                  |
| `[server].webhook_host`             | Public URL for webhook endpoint (catch-up matching)  | required                |
| `[server].max_body_size`            | Request body limit (bytes)                           | `1048576`               |
| `[server].catch_up_enabled`         | Replay missed events on startup                      | `true`                  |
| `[server].catch_up_max_age_hours`   | Max age (hours) of events to replay                  | `24`                    |

**Workflow file fields:**

| Field                       | Purpose                                                                  | Default  |
|-----------------------------|--------------------------------------------------------------------------|----------|
| `[trigger].type`            | Event type (e.g. `github_issue_assigned`, `gitlab_merge_request_review`) | required |
| `[trigger].allowed_users`   | **SECURITY BOUNDARY**: which usernames are permitted to trigger this workflow | required |
| `[git].clone`               | Whether to git clone the repo                                            | `false`  |
| `[git].shallow_clone`        | Whether to create a per-event shallow clone                              | `false`  |
| `[git].default_branch`      | Branch for shallow clone fallback                                         | `"main"` |
| `[[steps]].name`            | Human-readable step label                                                | required |
| `[[steps]].agent`           | Name of agent from `config.toml`                                         | required |
| `[[steps]].prompt_template` | `{{variable}}` template                                                  | required |
| `[[steps]].pre_hooks`       | Hooks to check before step                                               | none     |
| `[[steps]].post_hooks`      | Hooks to check after step                                                | none     |

Trigger-specific event-content filters (`assigned_to`, `mentioned_user`) are defined in **Appendix A: Trigger Reference** — each filter applies only to trigger types that support it.

## 4. Event Sources (Webhooks)

Yoke runs a single webhook handler, determined by the `platform` setting in `config.toml`. The handler is registered at `POST /webhook`.

### GitHub Webhooks

When `platform = "github"`, the handler at `POST /webhook` receives GitHub webhook deliveries. Each delivery includes:

- `X-GitHub-Event` header — the event type (`issues`, `issue_comment`, `pull_request_review`, `pull_request_review_comment`)
- `X-GitHub-Delivery` header — unique delivery UUID for this webhook delivery (used for watermark tracking)
- `X-Hub-Signature-256` header — HMAC-SHA256 signature for verification
- `WEBHOOK_SECRET` env var provides the HMAC-SHA256 key
- JSON payload — the event data

### GitLab Webhooks

When `platform = "gitlab"`, the handler at `POST /webhook` receives GitLab webhook deliveries. Each delivery includes:

- `X-GitLab-Event` header — the event type (`Issue Hook`, `Note Hook`)
- `X-Gitlab-Token` header — static token for verification
- `WEBHOOK_SECRET` env var provides the token value compared against this header
- JSON payload — the event data

### Verification

| Platform | Header                | Mechanism                                                                          |
|----------|-----------------------|------------------------------------------------------------------------------------|
| GitHub   | `X-Hub-Signature-256` | HMAC-SHA256 of the request body with the `WEBHOOK_SECRET` env var                  |
| GitLab   | `X-Gitlab-Token`      | Constant-time comparison of the header value against the `WEBHOOK_SECRET` env var |

Unverified payloads receive a `401` response and are logged as a warning. This prevents forgery and ensures the daemon only processes legitimate events.

See **Appendix A: Trigger Reference** for the complete mapping of trigger types to platform events and available template variables.

### Webhook Reliability

Both platforms retry webhook deliveries if the endpoint doesn't return 2xx:

**GitHub**: Retries up to 3 times with increasing delays (roughly 5s, 15s, 45s). Provides resilience against brief restarts and momentary overload.

**GitLab**: Retries up to 4 times with exponential backoff (up to ~50s between attempts for self-hosted; GitLab.com uses similar logic). Provides equivalent resilience.

For longer outages, the platform marks the delivery as failed and stops retrying. Yoke's **catch-up** feature addresses this: on startup, it queries the platform's delivery/events API to replay missed events that occurred while Yoke was offline. See the [Catch-Up (Event Replay)](#catch-up-event-replay) section in the README for configuration details.

## 5. HTTP Server

### Stack

- **axum** as the HTTP framework (lightweight, tokio-native, good ecosystem)
- **tower** middleware for logging, CORS (if needed), and request body limits

### Endpoints

| Method | Path       | Purpose                                                           |
|--------|------------|-------------------------------------------------------------------|
| POST   | `/webhook` | Receive platform webhook deliveries                               |
| GET    | `/health`  | Health check (returns `{"status": "ok"}`)                         |
| GET    | `/ready`   | Readiness check (returns 200 when dispatcher is accepting events) |

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
8. If a trigger matches, handler builds a `TriggerEvent` (including platform-specific delivery IDs as extra variables) and sends it through the mpsc channel
9. Returns `200` immediately — the handler never blocks on workflow execution

## 6. Dispatcher

The dispatcher consumes `DispatchMessage`s from the mpsc channel, manages dedup sets (in_flight, completed, permanently_failed), tracks per-repository watermarks for resume-after-restart, and throttles concurrency via a tokio semaphore. It is a pure consumer — it processes events queued by the webhook handler.

### Dedup Logic

- Dedup keys use the canonical `event_id` from `TriggerEvent.event_id` (as defined per trigger type in Appendix A). The key format is `{owner}/{repo}/{event_id}` — e.g. `mintybasil/yoke/issue-42`.
- Completed events are skipped
- In-flight events are skipped
- Permanently-failed events are skipped
- `[runtime].max_concurrent` from `config.toml` sets the semaphore capacity. 0 = unlimited.
- `completed.json` and `failed.json` with atomic writes for persistence.

### Concurrency Model

```
┌──────────────────┐  ┌──────────────────┐
│  Webhook Handler │  │  Signal Handler  │
│  (axum route)    │  │  (SIGINT/SIGTERM)│
└────────┬─────────┘  └────────┬─────────┘
         │ mpsc                │ watch
         ▼                     ▼
┌────────────────────────┐  ┌──────────────────────┐
│      Dispatcher        │  │  (shutdown signal)   │
│  (single consumer)     │  └──────────────────────┘
│  - dedup check         │
│  - semaphore acquire   │
│  - spawn workflow task │
│  - track in_flight     │
│  - drain on shutdown   │
└───────────┬────────────┘
            │ per event (tokio::spawn)
            ▼
┌────────────────────────┐
│  Workflow Runner (N)   │
│  - Git ops             │
│  - Step execution      │
│  - Hermes API call     │
│  - Cleanup             │
└────────────────────────┘
```

The dispatcher loop runs as a single tokio task, so the dedup check + in_flight insert is sequential (no races). Workflow runners are spawned as independent tokio tasks.

### Dispatch Flow

When the dispatcher consumes a `DispatchMessage`, it follows these steps in order:

1. **Dedup check**: Build the `{owner}/{repo}/{event_id}` dedup key using the canonical `event_id` from `TriggerEvent.event_id` (see Appendix A for formats) and check against `in_flight`, `completed`, and `permanently_failed` sets. If the event is already known, skip it.
2. **Authorized-actor check**: The dispatcher extracts the actor from the webhook payload (the user who performed the action, e.g. the person who assigned the issue) and checks it against the workflow's `allowed_users`. If the actor is not in the list, the workflow is skipped. This is a security boundary, not a content filter. (See the **Trigger Authorization** section for details on how the actor is determined per trigger type.)
3. **Semaphore acquire**: If the event is new and authorized, acquire a permit from the concurrency semaphore (or proceed immediately if `max_concurrent = 0`).
4. **Track in_flight**: Insert the event key into the in_flight set.
5. **Spawn workflow task**: Spawn a tokio task to run the workflow. On successful completion, the watermark for the event's repository is updated with the delivery/event ID and timestamp, then persisted to `watermark.json`.

## 7. Workflow Engine

### Step Structure

Each step has:
- `name` — human-readable label (used in log file names)
- `agent` — name of the Hermes API instance to use (references `[[agents]]` in `config.toml`)
- `prompt_template` — `{{variable}}` template rendered with event + global variables
- `pre_hooks` — optional list of hooks to check before running the step
- `post_hooks` — optional list of hooks to check after running the step

The `agent` field on each step allows different steps in the same workflow to target different Hermes profiles. Hermes requires a distinct API server for each profile.

### Template Variables

Prompt templates have access to:

1. **Global variables** — available in all triggers (see **Appendix A: Trigger Reference**)
2. **Trigger-specific variables** — extracted from the event payload (see **Appendix A: Trigger Reference**)

At startup, Yoke validates all prompt templates:

- **Variable existence**: Each `{{variable}}` placeholder is checked against the known set of global and trigger-specific variables. Unknown variables cause a hard exit.
- **Syntax errors**: Malformed placeholders (e.g., `{{variable`, `{{ }}`) are rejected.
- **Empty templates**: Templates that are empty or whitespace-only after rendering are flagged.

This catches user error early, before any webhook is received.

### Hooks

Pre/post step hooks validate file conditions before and after each step. A hook failure stops the workflow with a clear error message.

Hooks are configured per-step as inline TOML tables with a `type` field and hook-specific parameters:

```toml
[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan the issue"
pre_hooks = [{ type = "file_not_empty", path = "plan.md" }]
post_hooks = [{ type = "file_contains", path = "plan.md", text = "implementation" }]
```

| Hook             | Fields          | Checks                                  | Failure message                              |
|------------------|-----------------|-----------------------------------------|-----------------------------------------------|
| `file_not_empty`  | `path`          | File exists and has non-zero content    | `File 'X' is empty` or `File 'X' not found`  |
| `file_contains`   | `path`, `text`  | File contains the specified substring   | `File 'X' does not contain 'Y'`               |

### Dedup & Persistence

- **completed.json** — set of `{owner}/{repo}/{event_id}` strings (using canonical `event_id`) for events that completed successfully
- **failed.json** — array of `{key, timestamp, error}` entries for events that failed
- Atomic file writes (write to `.tmp`, rename)
- Loaded on startup, appended to on completion/failure

### Watermark Persistence

Watermarks track the last-processed webhook delivery per repository, enabling resume-after-restart semantics. On restart or after downtime, Yoke (or an external orchestration tool) can use watermark data to query the platform's API for events newer than the recorded watermark, processing anything that was missed.

**Data structures:**

- **`Watermark`** — a per-repository record with three fields:
  - `last_delivery_id: Option<String>` — the GitHub delivery UUID from the `X-GitHub-Delivery` header (GitHub only)
  - `last_event_id: Option<String>` — the GitLab event identifier from the webhook payload (GitLab only)
  - `last_processed_at: DateTime<Utc>` — the UTC timestamp when Yoke last processed an event for this repository

- **`WatermarkStore`** — a `HashMap<String, Watermark>` keyed by `{owner}/{repo}` (e.g. `mintybasil/yoke`). Wrapped in `Arc<RwLock<...>>` for thread-safe access.

**When watermarks are updated:**

On every successful workflow completion, `spawn_workflow` updates the watermark for the event's repository. The platform-specific delivery or event ID is sourced from dedicated `TriggerEvent` fields:

| Platform    | Source field              | Header / payload field      |
|-------------|--------------------------|-----------------------------|
| GitHub      | `delivery_id`            | `X-GitHub-Delivery` header  |
| GitLab      | `event_id`               | Payload event identifier    |

The `delivery_id` field on `TriggerEvent` carries the GitHub delivery UUID (extracted from the `X-GitHub-Delivery` header in the server's webhook handler); GitLab currently sets it to `None`. The `event_id` field holds the canonical event identifier used for deduplication and is the same value written to `last_event_id`. The `last_processed_at` field is set to `Utc::now()` at the time of the update. Only one field (`last_delivery_id` for GitHub, `last_event_id` for GitLab) is populated per platform — the other remains `None`.

**Persistence:**

- `WatermarkStore::persist()` writes the entire watermark map to `watermark.json` in the work directory, using the same atomic write pattern as dedup files (`.tmp` + `rename`)
- Empty stores skip file creation entirely (no unnecessary I/O)
- On startup, `load_watermarks()` reads `watermark.json`, falling back to an empty default for missing or corrupted files (matching the resilience pattern of dedup persistence)
- Watermarks are persisted alongside completed/failed dedup sets during graceful shutdown via `persist_state()`

**Example `watermark.json`:**

```json
{
  "mintybasil/yoke": {
    "last_delivery_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
    "last_event_id": null,
    "last_processed_at": "2026-06-08T17:30:00.123456Z"
  },
  "example-corp/backend-service": {
    "last_delivery_id": null,
    "last_event_id": "gitlab-event-42",
    "last_processed_at": "2026-06-08T17:25:00.654321Z"
  }
}
```

**Catch-up:** Watermarks provide the foundation for Yoke's catch-up mode. On startup, Yoke reads the persisted watermark for each repository and queries the platform's delivery/events API for events newer than the watermark timestamp, up to `catch_up_max_age_hours` old. This replays events that were delivered while Yoke was offline. See **Section 4 (Webhook Reliability)** for details.

## 8. Agents (Hermes API Harness)

Agents are named Hermes API instances defined in `config.toml`. Workflow files reference an agent by name, keeping deployment-specific connection details out of reusable workflow definitions.

### Agent Configuration (config.toml)

```toml
[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"
```

### Request Format

When the harness executes a step, it builds a request to the agent's `base_url`:

```json
{
  "instructions": "All work is in: /path/to/workspace. Always run `cd /path/to/workspace` as your first action before any file or terminal operations. Reference all file paths relative to this directory.",
  "input": "<rendered prompt>",
  "store": true
}
```

**Instructions field construction:**

- When `git.clone = true` or `git.shallow_clone = true`: The `instructions` field includes the workspace directory path with an explicit `cd` directive
- When both are `false`: The `instructions` field omits the workspace path (agent operates without local file access)

The workspace directory is `{workdir}/{owner}/{repo}/{event_id}/` (or `{workdir}/{owner}/{repo}/{event_id}/repo/` if shallow clone is enabled), where `{event_id}` is the canonical form from `TriggerEvent.event_id` (see Appendix A).

### Agent Resolution

At startup, every step's `agent` field is resolved against the `[[agents]]` array in `config.toml`. If any step references an agent name that doesn't match a configured agent, Yoke exits with a hard error. This ensures misconfigured workflows fail immediately.

### Conventions

- Uses `/v1/responses` endpoint
- `base_url` is host-only — the internal path `/v1/responses` is a constant in code
- Auth via `HERMES_API_KEY` env var (checked per invocation, never in config)
- `instructions` carries workspace path with explicit `cd` directive when git is enabled
- Response parsing extracts `output[].content[].type == "output_text"` blocks
- `HarnessConfig` is a single struct (not an enum)

## 9. Git & Shallow Clone Management

Yoke manages the git lifecycle for each configured repo:

1. **Shallow clone** — Per-event isolated clone via `git clone --depth=1 -b <branch>`, for events whose workflow has `[git] shallow_clone = true`
2. **Branch resolution** — prefers `TriggerEvent.branch` (populated by the webhook handler), falls back to the workflow's `git.default_branch`
3. **Cleanup** — the per-event clone directory is simply deleted after the workflow completes

The git orchestration is performed by the `Dispatcher` in `spawn_workflow()` before the `WorkflowRunner` is spawned. Git orchestration is **config-driven**: when any matching workflow has `[git] clone = true` or `[git] shallow_clone = true`, the dispatcher performs the corresponding git operations. This is not limited to review triggers — any workflow type can opt into git features via its `[git]` config section. The dispatcher:

1. Determines the platform and authentication token from the event
2. Resolves the branch: prefers `TriggerEvent.branch`, falls back to the workflow's `git.default_branch`
3. Calls `git::shallow_clone()` to perform a per-event `git clone --depth=1 -b <branch>` into the event workspace

Each event gets its own fully isolated shallow clone — no shared `.git` state, no concurrency conflicts. If any git operation fails, the event is marked as permanently failed and the workflow is not spawned.

Authentication via git2 `RemoteCallbacks` with token-based credentials. The token env var is determined by the `platform` setting:

| Platform | Env Var        | Clone URL Pattern                                              |
|----------|----------------|----------------------------------------------------------------|
| GitHub   | `GITHUB_TOKEN` | `https://x-access-token:{token}@github.com/{owner}/{repo}.git` |
| GitLab   | `GITLAB_TOKEN` | `https://oauth2:{token}@{gitlab_host}/{owner}/{repo}.git`      |

Where `gitlab_host` is `gitlab.com` by default, or the value of `gitlab_url` for self-hosted instances.

Token never embedded in URLs or git config stored persistently — only used in the clone/pull `RemoteCallbacks`.

The Hermes API agent receives the clone path via the `instructions` field and uses `cd <path>` as its first action. If the path doesn't exist or isn't accessible, the agent falls back to the platform's file API via MCP tools (GitHub Contents API or GitLab Repository Files API).

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
5. State is persisted (completed.json, failed.json, and watermark.json updated)
6. Process exits
7. Second signal: immediate `process::exit(1)`

## 11. Data Directory Layout

```
{workdir}/
  completed.json              # Set of completed event keys
  failed.json                 # Array of failure entries
  watermark.json              # Per-repo watermark: last delivery ID, last event ID, last processed timestamp
  {owner}/{repo}/
    repo/                     # per-event shallow clone (when git.shallow_clone = true)
    {event_id}/           # per-event workspace
      00_Plan.log             # Full Hermes API request + response, with final message rendered
      00_Plan.prompt          # Rendered prompt for auditing
      01_Implement.log
      01_Implement.prompt
```

`XX_<name>.log` contains the full HTTP exchange: the request body sent to Hermes API, the response received, and the extracted final message (from `output[].content[].type == "output_text"`) rendered in a human-readable format at the end of the file.

## 12. Error Handling

Two-tier model: startup errors are hard exits, runtime errors are per-event soft failures.

### Startup Hard Exits

- Missing `config.toml` or invalid TOML
- Missing `platform` field
- Invalid `platform` value (must be `"github"` or `"gitlab"`)
- Unknown agent name in a workflow file (doesn't match any `[[agents]]` entry)
- Missing platform token env var (`GITHUB_TOKEN` for github, `GITLAB_TOKEN` for gitlab)
- Missing `HERMES_API_KEY` env var
- Invalid `agents[].base_url` (must be a valid HTTP URL)
- Missing `WEBHOOK_SECRET` env var
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
- Clone cleanup failure → logged, workflow result preserved

## 13. Module Map

| File                     | Responsibility                                                                      |
|--------------------------|-------------------------------------------------------------------------------------|
| `src/main.rs`            | Entry point: startup validation, tracing init, server + dispatcher + signal handler |
| `src/config.rs`          | Config struct (TOML): config.toml + workflow files, clap CLI                        |
| `src/server.rs`          | axum HTTP server: router, middleware, health endpoint                               |
| `src/webhook/mod.rs`     | Webhook handler dispatch: selects GitHub or GitLab handler based on `platform`      |
| `src/webhook/github.rs`  | GitHub webhook handler: HMAC-SHA256 verify, payload parse, event mapping            |
| `src/webhook/gitlab.rs`  | GitLab webhook handler: token verify, payload parse, event mapping                  |
| `src/dispatcher.rs`      | Concurrency control: dedup sets, semaphore, mpsc consumer, persistence              |
| `src/runner.rs`          | Per-event workflow execution: git ops, step loop, template rendering                |
| `src/harness.rs`         | Hermes API client: request building, response parsing                               |
| `src/git.rs`             | Git shallow clone management: clone, auth, status checks                          |
| `src/hooks.rs`           | Hook enum + run_hook() dispatcher                                                   |
| `src/template.rs`        | `{{key}}` placeholder renderer                                                      |
| `src/workflow.rs`        | Workflow definition, trigger types, loading & validation                           |
| `src/cli.rs`             | CLI argument parsing with clap                                                      |
| `src/logging.rs`         | Per-event workflow step log file writing                                           |
| `src/reload.rs`          | Hot-reload file watcher & atomic workflow state swap                                |

## 14. CLI

```bash
yoke [OPTIONS]

Options:
  --config <FILE>              Path to config.toml (default: ./config.toml)
  --workflows <DIR>            Directory containing workflow TOML files (default: ./workflows)
  --host <ADDR>                Server bind address (overrides config.toml)
  --port <PORT>                Server listen port (overrides config.toml)
```

`--host` and `--port` override `config.toml` values. `[runtime].max_concurrent`, `[runtime].workdir`, and `platform` are set in `config.toml` (no CLI flags).

## 15. Environment Variables

| Variable         | Purpose                                                   | Required                   |
|------------------|-----------------------------------------------------------|----------------------------|
| `GITHUB_TOKEN`   | GitHub authentication for git clone/pull                  | When `platform = "github"` |
| `GITLAB_TOKEN`   | GitLab authentication for git clone/pull                  | When `platform = "gitlab"` |
| `HERMES_API_KEY` | Bearer token for Hermes REST API                          | Yes                        |
| `WEBHOOK_SECRET` | Webhook auth key (GitHub HMAC key or GitLab token)        | Yes                        |

## 16. Example Configs

### config.toml (GitHub)

```toml
platform = "github"

repos = [
    { owner = "example-corp", repo = "backend-service" },
    { owner = "example-corp", repo = "frontend-app" },
]

[[agents]]
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"

[runtime]
max_concurrent = 2
workdir = "~/.yoke"

[server]
host = "0.0.0.0"
port = 8644
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
name = "pm"
base_url = "http://localhost:8000"

[[agents]]
name = "swe"
base_url = "http://localhost:8001"

[runtime]
max_concurrent = 2
workdir = "~/.yoke"

[server]
host = "0.0.0.0"
port = 8644
```

### Workflow: GitHub issue plan+implement

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "alice"              # Event-content filter: only when alice is assigned
allowed_users = ["bob"]            # Authorization: only bob may trigger this workflow

[git]
clone = true
shallow_clone = true

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = """
Plan the implementation for {{owner}}/{{repo}}#{{issue_number}}.
Save the plan to {{output_dir}}/plan.md
"""

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = """
Read the plan at {{output_dir}}/plan.md and implement it for {{owner}}/{{repo}}#{{issue_number}}.
Create a PR with your changes.
"""
```

### Workflow: GitHub PR review response

```toml
[trigger]
type = "github_pull_request_review"
allowed_users = ["alice"]           # Authorization: only alice may trigger this workflow

[git]
clone = true
shallow_clone = true

[[steps]]
name = "Address Review"
agent = "pm"
prompt_template = """
Address the review feedback on {{owner}}/{{repo}}#{{pr_number}}.
Review ID: {{review_id}}
"""
```

This trigger has no `assigned_to` or `mentioned_user` filter — it fires on any PR review submission, but only if the review author (the actor) is `alice`. The actor for a review event is the person who submitted the review.

### Workflow: GitLab issue plan+implement

```toml
[trigger]
type = "gitlab_issue_assigned"
assigned_to = "alice"              # Event-content filter: only when alice is assigned
allowed_users = ["bob"]            # Authorization: only bob may trigger this workflow

[git]
clone = true
shallow_clone = true

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = """
Plan the implementation for {{owner}}/{{repo}}#!{{issue_iid}}.
Save the plan to {{output_dir}}/plan.md
"""

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = """
Read the plan at {{output_dir}}/plan.md and implement it for {{owner}}/{{repo}}#!{{issue_iid}}.
Create an MR with your changes.
"""
```

### Workflow: GitLab merge request review response

```toml
[trigger]
type = "gitlab_merge_request_comment_mention"
mentioned_user = "alice"           # Event-content filter: only when alice is @mentioned
allowed_users = ["bob"]            # Authorization: only bob may trigger this workflow

[git]
clone = true
shallow_clone = true

[[steps]]
name = "Address Review"
agent = "pm"
prompt_template = """
Address the review feedback on {{owner}}/{{repo}}#!{{mr_iid}}.
Review ID: {{review_id}}
"""
```

## 17. Design Decisions (Resolved)

1. **Single platform per instance with unified webhook path**: Yoke handles one platform (GitHub or GitLab) per instance, set globally in `config.toml`. A single `POST /webhook` endpoint serves that platform — only one handler is active at a time, selected by the `platform` setting. There is no ambiguity about which verification and parsing logic to apply. Supporting both platforms in a single instance adds complexity across config, routing, dedup, authentication, and data layout for a marginal use case. Running two instances with separate configs is simpler to operate and reason about.

2. **Platform-specific trigger types**: Trigger types carry the platform prefix (e.g., `github_issue_assigned`, `gitlab_merge_request_review`). GitHub and GitLab have different event models, payload shapes, and action semantics — unified names paper over real differences and create ambiguous mappings. Prefixed types make workflows explicit about which platform they target. At startup, any workflow containing a trigger type that doesn't match the configured platform is rejected with a hard exit, catching misconfigured workflows immediately.

3. **Payload size limit**: Configurable via `[server].max_body_size`. Default 1MB. GitHub and GitLab payloads are typically <100KB, but large diffs can exceed that. Users who hit the limit can increase it.

4. **HTTPS/TLS**: Reverse proxy is the expected pattern. The HTTP server listens on plain HTTP. For production, put it behind Caddy, nginx, or a cloudflare tunnel. TLS termination is not Yoke's job — it's infrastructure.

5. **Webhook secret rotation**: Restart required. Changing the secret in the platform's webhook settings, updating the `WEBHOOK_SECRET` env var, and then restarting Yoke is a simple, reliable workflow. Hot-reloading secrets adds complexity (race conditions between the old and new secret during rotation) for marginal benefit.

6. **Multi-workflow dedup**: Shared dedup sets. If two workflows match the same event (e.g., both `github_issue_assigned` for overlapping repos), the first workflow loaded runs. Per-workflow dedup would require tracking completed events per-workflow-file, which doubles the persistence complexity for a marginal use case. If this becomes a problem, the user should scope triggers more tightly.

7. **GitLab token verification**: GitLab webhook verification uses a static token in the `X-Gitlab-Token` header, not HMAC. This is GitLab's standard mechanism. The comparison is done in constant time to prevent timing attacks.

8. **Config separation**: Global user settings (`platform`, `repos`, `agents`, `[runtime]`, `[server]`) live in `config.toml`. Workflow definitions (`[trigger]`, `[git]`, `[[steps]]`) live in separate `.toml` files. Each step carries its own `agent` reference, keeping deployment-specific connection details out of workflow definitions while allowing steps to target different agents.

9. **Named agents**: `[[agents]]` in `config.toml` defines named Hermes API instances. Each step in a workflow references an agent by name (`agent = "pm"`), keeping `base_url` out of workflow files and making it easy to retarget a step by changing the config.

10. **Shared repos**: All repos in `config.toml` share the same workflow files. This simplifies the mental model — adding a new repo means one entry in the `repos` array, and every existing workflow automatically applies. Event-content filters (`assigned_to`, `mentioned_user`) scope which events match; `allowed_users` is a SECURITY BOUNDARY that controls who may invoke the workflow.

11. **Step-level agent assignment**: Each step declares its own `agent` field rather than a single workflow-level agent. This allows a workflow to use different Hermes API instances for different steps (e.g., a planning step on the pm agent, an implementation step on the swe agent).

12. **Hermes-only agent config**: The `[[agents]]` config contains only `name` and `base_url`. Provider and model selection are Hermes Agent internals — Yoke sends `instructions` and `input` to `/v1/responses`, and Hermes handles provider routing and model selection.

13. **Assignment-only issue triggers**: The `github_issue_assigned` and `gitlab_issue_assigned` trigger types fire on the assignment event only, not on issue open. "Issue opened" is a semantically distinct event (the issue exists but no one is responsible for acting on it yet) and warrants its own trigger type if needed in the future. Conflating the two would require workflows to handle two different contexts (newly filed vs. explicitly assigned) in the same template logic.

14. **GitLab review triggers mirror GitHub**: GitLab does not have separate webhook events for "review submitted" vs. "inline review comment" — both arrive as `Note Hook` events. Yoke splits them into `gitlab_merge_request_review` (any Note on a MergeRequest) and `gitlab_merge_request_review_comment` (DiffNote on a MergeRequest) to maintain naming parity with GitHub's `github_pull_request_review` and `github_pull_request_review_comment`. The split is implemented by inspecting the `noteable_type` and `type` fields in the Note Hook payload. This gives workflow authors a consistent trigger vocabulary across platforms, even though the underlying webhook mechanism differs.

## 18. Operations

### Webhook Management CLI

Yoke provides CLI subcommands to configure and remove webhooks on the platform. These commands are idempotent and safe to run multiple times.

When setting up webhooks, the CLI inspects the workflows and subscribes only to the event types actually used by 
configured triggers.

```bash
# Configure webhooks for all repos in config.toml
yoke webhooks add --config config.toml --workflows ./workflows

# Remove webhooks (e.g., before decommissioning)
yoke webhooks remove --config config.toml

# List configured webhooks (idempotency check)
yoke webhooks list --config config.toml
```

**`webhooks add` behavior:**

For each repo in `config.toml`:
1. Check if a webhook exists with matching URL (`https://your-yoke-host/webhook`)
2. If yes: update the webhook secret and event subscriptions
3. If no: create a new webhook with the configured secret
4. Report summary: created/updated/skipped count

**`webhooks remove` behavior:**

For each repo in `config.toml`:
1. Find webhooks matching Yoke's URL
2. Delete matching webhooks
3. Report summary: deleted count

**`webhooks list` behavior:**

For each repo in `config.toml`:
1. Fetch all webhooks
2. Display: URL, secret (last 4 chars only), events subscribed, active status
3. Highlight which webhooks match Yoke's configuration

### Initial Setup

1. Deploy Yoke and note its public URL (e.g., `https://yoke.example.com`)
2. Set the `WEBHOOK_SECRET` environment variable
3. Run `yoke webhooks add --config config.toml --workflows ./workflows`
4. Verify with `yoke webhooks list --config config.toml`
5. Start Yoke daemon

The `webhooks add` command configures the platform to send events for the specific event types Yoke handles:

**GitHub events:**
- `issues` (action: `assigned`)
- `issue_comment` (action: `created`)
- `pull_request_review` (action: `submitted`)
- `pull_request_review_comment` (action: `created`)

**GitLab events:**
- `Issue Hook`
- `Note Hook`

### Secret Rotation

To rotate the webhook secret:

1. Generate a new secret (e.g., `openssl rand -hex 32`)
2. Update the `WEBHOOK_SECRET` environment variable
3. Run `yoke webhooks add --config config.toml` — this updates the secret on all platform webhooks
4. Restart Yoke to load the new secret

There is no grace period — the secret changes atomically. If the restart fails, the platform will be sending webhooks with the new secret but Yoke will reject them (logged as 401). Roll back by reverting the `WEBHOOK_SECRET` env var and running `webhooks add` again.

### Decommissioning

Before shutting down a Yoke instance permanently:

1. Run `yoke webhooks remove --config config.toml`
2. Verify with `webhooks list` that webhooks are deleted
3. Stop Yoke daemon

This prevents the platform from continuing to send webhooks to a dead endpoint, which would fill up delivery failure logs.

### Troubleshooting

**Webhooks not being received:**

1. Run `webhooks list` to verify webhooks exist and are active
2. Check platform delivery logs (GitHub: repo Settings → Webhooks → Recent Deliveries; GitLab: Settings → Webhooks → Recent Events)
3. Look for non-2xx responses or timeouts
4. Verify Yoke's public URL is reachable (test with `curl -X POST https://your-host/webhook`)

**401 errors in Yoke logs:**

- Secret mismatch between `WEBHOOK_SECRET` env var and platform webhook configuration
- Run `webhooks add` to sync the secret, then restart

**503 Service Unavailable:**

- Dispatcher is overwhelmed (all semaphore slots in use)
- Increase `[runtime].max_concurrent` or reduce webhook event volume

**Missing events:**

- Check if the event type + action matches a configured trigger
- Verify the repo is listed in `config.toml`
- Check platform delivery logs for failed deliveries (platform may have stopped retrying)

## 19. Testing

### Testing Strategy

Yoke is tested at three levels:

1. **Unit tests** — Individual components (template rendering, webhook parsing, dedup logic)
2. **Integration tests** — HTTP server, webhook handling, workflow execution with mocked Hermes API
3. **End-to-end tests** — Full event flow with realistic payloads and temporary workspaces

### Unit Tests

**Coverage targets:**

| Module              | What to Test                                                                |
|---------------------|-----------------------------------------------------------------------------|
| `template.rs`       | Variable substitution, missing variables (panic), nested braces `{{{var}}}` |
| `webhook/github.rs` | HMAC verification (valid/invalid), payload parsing, event mapping           |
| `webhook/gitlab.rs` | Token verification (valid/invalid), payload parsing, event mapping          |
| `dispatcher.rs`     | Dedup logic (same key skipped, different keys run), semaphore acquisition   |
| `hooks.rs`          | `Hook` enum, `run_hook()`, `HookError` — all hook types (pass/fail cases), TOML deserialization/serialization                         |
| `config.rs`         | TOML parsing (valid/invalid), agent resolution, trigger validation          |

**Example: Template rendering**

```rust
#[test]
fn test_template_render_unknown_variable_panics() {
    let template = "Hello {{unknown_var}}";
    let vars = HashMap::from([("known", "value")]);
    // Should panic with clear error message
    assert_render_panic(template, &vars, "unknown variable: unknown_var");
}

#[test]
fn test_template_render_nested_braces() {
    let template = "Issue: {{{issue_body}}}";  // {{{var}}} = literal { + {{var}}
    let vars = HashMap::from([("issue_body", "Fix the bug")]);
    let result = render(template, &vars);
    assert_eq!(result, "Issue: {Fix the bug}");
}
```

### Integration Tests

**Test harness setup:**

```rust
// tests/common/mod.rs
pub struct TestHarness {
    pub server: ServerHandle,
    pub mock_hermes: MockHermesServer,
    pub temp_dir: TempDir,
    pub config: Config,
}

pub async fn setup() -> TestHarness {
    // Start mock Hermes API on random port
    // Create temp config.toml pointing to mock
    // Start Yoke server
    // Return handles for cleanup
}
```

**Test cases:**

| Test                                 | Setup                             | Assert                                    |
|--------------------------------------|-----------------------------------|-------------------------------------------|
| `webhook_valid_github_issue`         | POST valid GitHub issue payload   | 200, workflow started, Hermes called once |
| `webhook_invalid_signature`          | POST with wrong HMAC              | 401, no workflow started                  |
| `webhook_duplicate_skipped`          | POST same issue twice             | Second returns 200, no second workflow    |
| `webhook_no_matching_trigger`        | POST event type not in workflows  | 200 (no-op), no workflow started          |
| `hermes_api_failure`                 | Mock Hermes returns 500           | Workflow fails, `.error` file written     |
| `concurrent_workflows_respect_limit` | Send N webhooks, max_concurrent=2 | At most 2 run simultaneously              |

**Example: Webhook handling**

```rust
#[tokio::test]
async fn test_webhook_valid_github_issue() {
    let harness = setup().await;
    
    // Load fixture payload
    let payload = include_str!("fixtures/github_issue_assigned.json");
    
    // Send webhook
    let response = reqwest::Client::new()
        .post(format!("{}/webhook", harness.server.url()))
        .header("X-GitHub-Event", "issues")
        .header("X-Hub-Signature-256", compute_hmac(payload, &std::env::var("WEBHOOK_SECRET").unwrap()))
        .body(payload)
        .send()
        .await
        .unwrap();
    
    assert_eq!(response.status(), 200);
    
    // Wait for workflow to complete
    tokio::time::sleep(Duration::from_secs(2)).await;
    
    // Verify Hermes was called
    assert_eq!(harness.mock_hermes.request_count(), 1);
    let request = harness.mock_hermes.last_request();
    assert!(request.input.contains("Issue #42 has been assigned"));
    
    // Verify output files exist
    assert!(harness.temp_dir.path().join("00_Plan.log").exists());
}
```

### End-to-End Tests

**Fixture-based testing:**

Store real webhook payloads in `tests/fixtures/`:

```
tests/fixtures/
  github_issue_assigned.json
  github_pull_request_review.json
  gitlab_issue_assigned.json
  gitlab_merge_request_review.json
```

These are actual payloads copied from GitHub/GitLab webhook delivery logs (with sensitive data redacted). This ensures Yoke handles real-world payload structures, not just idealized test cases.

**Test workflow:**

1. Load fixture payload
2. Start Yoke with test config (single repo, mock Hermes)
3. Send webhook to `/webhook`
4. Poll for workflow completion (check `completed.json` or file existence)
5. Assert:
   - Hermes received correct number of requests
   - Request bodies match expected templates
   - Output files exist with expected content
   - Git operations completed (shallow clone created/cleaned up)

### Local Development Testing

**Testing with ngrok:**

```bash
# Start ngrok tunnel
ngrok http 8644

# Copy the ngrok URL (e.g., https://abc123.ngrok.io)

# Set required environment variables
export WEBHOOK_SECRET="dev-secret"

# Update config.toml
[server]
host = "0.0.0.0"
port = 8644

# Start Yoke
cargo run -- --config config.toml

# In another terminal, configure GitHub webhook:
# - Payload URL: https://abc123.ngrok.io/webhook
# - Secret: dev-secret
# - Events: Issues (assigned), Pull request reviews

# Trigger a test: assign yourself to an issue
```

**Testing with curl (no platform):**

```bash
# Load a fixture payload
curl -X POST http://localhost:8644/webhook \
  -H "Content-Type: application/json" \
  -H "X-GitHub-Event: issues" \
  -H "X-Hub-Signature-256: $(compute_hmac.sh fixture.json dev-secret)" \
  -d @tests/fixtures/github_issue_assigned.json

# Watch logs for processing
```

**Test helper script:**

```bash
#!/bin/bash
# scripts/test-webhook.sh

FIXTURE="$1"
SECRET="${2:-dev-secret}"
URL="${3:-http://localhost:8644/webhook}"

if [ -z "$FIXTURE" ]; then
  echo "Usage: test-webhook.sh <fixture.json> [secret] [url]"
  exit 1
fi

# Compute HMAC (requires openssl)
SIGNATURE=$(openssl dgst -sha256 -hmac "$SECRET" -binary < "$FIXTURE" | xxd -p -c 256)

curl -X POST "$URL" \
  -H "Content-Type: application/json" \
  -H "X-GitHub-Event: issues" \
  -H "X-Hub-Signature-256: sha256=$SIGNATURE" \
  -d "@$FIXTURE" \
  -v
```

### Mock Hermes API

The mock Hermes server is a minimal HTTP server that:

1. Accepts `POST /v1/responses` requests
2. Returns a fixed response with `output_text` content
3. Records all requests for assertion
4. Can be configured to fail (return 500) for error handling tests

```rust
// tests/mock_hermes.rs
pub struct MockHermesServer {
    requests: Arc<Mutex<Vec<Request>>>,
    fail_next: AtomicBool,
}

impl MockHermesServer {
    pub fn new() -> Self { ... }
    
    pub fn set_fail_next(&self, fail: bool) {
        self.fail_next.store(fail, Ordering::SeqCst);
    }
    
    pub fn request_count(&self) -> usize {
        self.requests.lock().len()
    }
    
    pub fn last_request(&self) -> Request {
        self.requests.lock().last().unwrap().clone()
    }
}

// Handler
async fn handle_response(
    State(server): State<Arc<MockHermesServer>>,
    Json(req): Json<serde_json::Value>,
) -> impl IntoResponse {
    server.requests.lock().push(Request::from_value(req));
    
    if server.fail_next.swap(false, Ordering::SeqCst) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Simulated failure");
    }
    
    Json(serde_json::json!({
        "output": [{
            "content": [{
                "type": "output_text",
                "text": "Work complete"
            }]
        }]
    }))
}
```

### CI Testing

**GitHub Actions workflow:**

```yaml
# .github/workflows/test.yml
name: Test

on: [push, pull_request]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      
      - name: Install Rust
        uses: dtolnay/rust-toolchain@stable
      
      - name: Cache cargo
        uses: Swatinem/rust-cache@v2
      
      - name: Run unit tests
        run: cargo test --lib
      
      - name: Run integration tests
        run: cargo test --test '*'
      
      - name: Run clippy
        run: cargo clippy -- -D warnings
      
      - name: Check formatting
        run: cargo fmt --all -- --check
```

**No external dependencies:**

CI tests should never require:
- Real GitHub/GitLab API access
- Real Hermes API
- Network access (except localhost)

All external dependencies are mocked or use fixture files.

### Performance Testing

**Load test with `oha` or `wrk`:**

```bash
# Generate 100 concurrent webhooks over 10 seconds
oha -c 100 -z 10s \
  -H "X-GitHub-Event: issues" \
  -H "X-Hub-Signature-256: sha256=abc123" \
  -d @tests/fixtures/github_issue_assigned.json \
  http://localhost:8644/webhook
```

**Metrics to track:**

- Request latency (p50, p95, p99)
- 503 rate (should be 0 unless intentionally overloaded)
- Time from webhook received to workflow started
- Memory usage under load

**Semaphore stress test:**

```rust
#[tokio::test]
async fn test_semaphore_overflow() {
    let harness = setup_with_max_concurrent(2).await;
    
    // Send 10 webhooks rapidly
    let mut handles = vec![];
    for _ in 0..10 {
        handles.push(send_webhook(&harness.server).await);
    }
    
    // All should complete (some with 503)
    let results = futures::future::join_all(handles).await;
    let status_codes: Vec<_> = results.iter().map(|r| r.status()).collect();
    
    // At least some should be 503 (dispatcher overwhelmed)
    assert!(status_codes.iter().filter(|&&s| s == 503).count() > 0);
    
    // But server should not crash
    assert!(harness.server.is_healthy());
}
```

### Test Data Management

**Fixture generation:**

When GitHub/GitLab change webhook payload structures, tests may break silently. Add a cron job or manual process to:

1. Capture real webhook deliveries from production
2. Redact sensitive data (tokens, emails, internal URLs)
3. Update fixture files
4. Run tests against new fixtures

**Fixture validation:**

```rust
#[test]
fn test_fixtures_are_valid_json() {
    for entry in std::fs::read_dir("tests/fixtures").unwrap() {
        let entry = entry.unwrap();
        if entry.path().extension() == Some("json".as_ref()) {
            let content = std::fs::read_to_string(entry.path()).unwrap();
            serde_json::from_str::<serde_json::Value>(&content)
                .unwrap_or_else(|e| panic!("Invalid JSON in {:?}: {}", entry.path(), e));
        }
    }
}
```

## 20. Catch-Up (Event Replay)

When Yoke starts up (or restarts after downtime), it needs to process webhook events that were delivered by the platform while Yoke was offline. This is the **catch-up** feature, implemented in `src/catch_up.rs`.

### How It Works

1. **Watermark loading**: On startup, `run_catch_up()` reads the last-processed timestamp per repository from `WatermarkStore` (persisted in `watermark.json`).

2. **Event retrieval**: For each configured repository, it queries the platform's delivery/events API for events newer than the watermark, up to `catch_up_max_age_hours` old:
   - **GitHub**: Finds the webhook matching `webhook_host` URL, then calls `list_deliveries` + `get_delivery` to fetch full delivery bodies. Decodes `payload_base64` and replays through `parse_github_event` + `map_to_trigger_event`.
   - **GitLab**: Calls `list_project_events(project_id, after=watermark_timestamp)` and replays through `ProjectEvent::try_into_trigger_event`.

3. **Dispatch**: Replayed events are sent over the same mpsc channel used by live webhooks. They arrive at the dispatcher as regular `DispatchMessage`s with the same structure.

### Interaction with Dedup

Because catch-up replays use the same canonical `event_id` format as live webhooks (e.g., `owner/repo/issue-42`), the dispatcher's existing three-set dedup mechanism naturally prevents duplicate processing when a live webhook and a replayed event overlap:

| Dedup Set | Effect on Replayed Event |
|-----------|--------------------------|
| `in_flight` | If a live webhook for the same event is currently being processed, the replayed event is skipped (no duplicate workflow run). |
| `completed` | If the event was already processed in a previous run, the replayed event is skipped (no re-processing). |
| `permanently_failed` | If the event permanently failed in a previous run, the replayed event is skipped (not retried). |

This means:
- **Concurrent live + replayed events** for the same logical event produce only one workflow run.
- **Already-completed events** from a previous run are silently skipped.
- **Permanently-failed events** are not retried on catch-up (they require manual intervention).

The `delivery_id` field (GitHub's `X-GitHub-Delivery` GUID for live webhooks, or the delivery GUID for replayed events) is stored on `TriggerEvent` for **watermark tracking only** — it is NOT used as a dedup key. Dedup always uses the canonical `event_id`.

### Configuration

| Setting | Default | Description |
|---------|---------|-------------|
| `catch_up_enabled` | `true` | Enable/disable catch-up on startup |
| `catch_up_max_age_hours` | `24` | Maximum age of events to replay |

Both are in the `[server]` section of `config.toml`.

### CatchUpSummary

`CatchUpSummary` tracks replayed/skipped/errored counts per repository and displays a human-readable summary via `Display`. This is logged at startup so operators can see what catch-up did.

## Appendix A: Trigger Reference

This appendix consolidates all trigger types, event mappings, and template variables for both platforms.

`allowed_users` is a **SECURITY BOUNDARY** that applies to every trigger type — it restricts which usernames are permitted to trigger a workflow. The actor checked against `allowed_users` is the user who performed the action (see the **Actor Source** column), not the assignee or mentioned user.

**Event Filters** are required for trigger types that support them (`—` = not applicable).

### GitHub Triggers

| Trigger Type                          | Event Header          | Action      | Variables                                               | Event Filters          | Actor Source                       | Event ID Format                             |
|---------------------------------------|-----------------------|-------------|---------------------------------------------------------|-------------------------|------------------------------------|---------------------------------------------|
| `github_issue_assigned`               | `issues`              | `assigned`  | `issue_number`, `assignee`, `issue_title`, `issue_body` | `assigned_to`            | `payload.sender.login`             | `issue-{issue_number}`                      |
| `github_issue_comment_mention`        | `issue_comment`       | `created`   | `issue_number`, `comment_id`, `comment_body`            | `mentioned_user`         | `payload.sender.login`             | `issue-{issue_number}-comment-{comment_id}` |
| `github_pull_request_review`          | `pull_request_review` | `submitted` | `pr_number`, `review_id`, `review_body`                 | —                        | `payload.review.user.login` (fallback `payload.sender.login`) | `pr-{pr_number}-review-{review_id}`         |
| `github_pull_request_comment_mention` | `issue_comment`       | `created`   | `pr_number`, `review_id`, `comment_id`, `comment_body`  | `mentioned_user`         | `payload.sender.login`             | `pr-{pr_number}-comment-{comment_id}`       |

_Note: Github considers PRs as a type of issue. `github_issue_comment` should only trigger for comments on issues, not 
PRs. For comments on PRs, the `github_pull_request_comment` trigger should receive them._

### GitLab Triggers

| Trigger Type                           | Event Header | Object Kind                           | Variables                                                               | Event Filters          | Actor Source               | Event ID Format                    |
|----------------------------------------|--------------|---------------------------------------|-------------------------------------------------------------------------|-------------------------|----------------------------|------------------------------------|
| `gitlab_issue_assigned`                | `Issue Hook` | `issue` (action: `update`)            | `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body` | `assigned_to`            | `payload.user.username`    | `issue-{issue_iid}`                |
| `gitlab_issue_mention`                 | `Note Hook`  | `note` (noteable_type = Issue)        | `issue_iid` `note_id`, `comment_body`                                   | `mentioned_user`         | `payload.user.username`    | `issue-{issue_iid}-note-{note_id}` |
| `gitlab_merge_request_review`          | `Note Hook`  | `note` (noteable_type = MergeRequest) | `mr_iid`, `review_id`, `review_body`                                    | —                        | `payload.user.username`    | `mr-{mr_iid}-review-{note_id}`     |
| `gitlab_merge_request_comment_mention` | `Note Hook`  | `note` (noteable_type = MergeRequest) | `mr_iid`, `note_id`, `comment_body`                                     | `mentioned_user`         | `payload.user.username`    | `mr-{mr_iid}-comment-{note_id}`    |

### Known Limitations

- Inline PR comments are not captured
- Multiple mentions on a single comment are not currently supported (event IDs will conflict)
