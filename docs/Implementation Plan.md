# Implementation Plan

This document outlines the phased implementation approach for Yoke. It complements the [Architecture Design](./Architecture%20Design.md) by focusing on build order and milestones rather than re-describing architectural components.

## Guiding Principles

1. **Startup validation first** — Hard exits on config errors before any runtime code
2. **Single platform MVP** — GitHub-only, then add GitLab behind the same `platform` flag
3. **Config-driven** — `config.toml` + workflow `.toml` files loaded before any HTTP server starts
4. **Dedup before concurrency** — Get correctness before adding the semaphore

---

## Phase 1: Config Loading & Validation

**Goal:** Parse and validate all configuration at startup with hard exits on errors

### Scope
- `config.toml` parsing: `platform`, `repos`, `[[agents]]`, `[runtime]`, `[server]`
- Workflow `.toml` parsing: `[trigger]`, `[git]`, `[[steps]]`
- Agent resolution: verify every `step.agent` matches a configured `[[agents]]` entry
- Trigger validation: verify trigger type prefix matches `platform`
- Template validation: check `{{variable}}` syntax and known variables
- CLI args: `--config`, `--workflows`, `--host`, `--port`
- Environment variable checks: `GITHUB_TOKEN`/`GITLAB_TOKEN`, `HERMES_API_KEY`, `WEBHOOK_SECRET`

### Deliverables
- [ ] `src/config.rs` — TOML structs with serde
- [ ] `src/workflow.rs` — Step type definition
- [ ] `src/template.rs` — Template renderer with validation
- [ ] `src/main.rs` — Startup sequence with validation
- [ ] Unit tests: invalid TOML, unknown agent, mismatched platform prefix, unknown template variables

### Exit Criteria
- Invalid `config.toml` exits with clear error message
- Unknown agent name in workflow exits at startup
- Trigger type with wrong platform prefix (e.g., `gitlab_*` when `platform = "github"`) exits
- Unknown `{{variable}}` in template exits
- Valid config loads successfully

---

## Phase 2: HTTP Server & Webhook Handler

**Goal:** Receive and verify webhook events, return 200/401 appropriately

### Scope
- axum server with `/health`, `/ready`, `/webhook` endpoints
- GitHub webhook handler: HMAC-SHA256 verification via `X-Hub-Signature-256`
- GitLab webhook handler: token verification via `X-Gitlab-Token`
- Platform selection via `config.platform`
- Payload parsing into structured event types
- Body size limit enforcement (`[server].max_body_size`)

### Deliverables
- [ ] `src/server.rs` — axum router, middleware, health endpoints
- [ ] `src/webhook/mod.rs` — dispatch to github.rs or gitlab.rs based on platform
- [ ] `src/webhook/github.rs` — HMAC verification, payload structs
- [ ] `src/webhook/gitlab.rs` — token verification, payload structs
- [ ] `src/webhooks/github.rs` — GitHub event payload types
- [ ] `src/webhooks/gitlab.rs` — GitLab event payload types
- [ ] Unit tests: valid/invalid signatures, payload parsing

### Exit Criteria
- Valid GitHub webhook with correct HMAC returns 200
- Invalid HMAC returns 401
- GitLab token verification works
- `/health` returns 200 with `{"status": "ok"}`
- Body size limit enforced

---

## Phase 3: Dispatcher & Dedup

**Goal:** Consume webhook events, deduplicate, spawn workflow runners

### Scope
- mpsc channel: webhook handler → dispatcher
- Dedup sets: `in_flight`, `completed`, `permanently_failed`
- Dedup key format: `{owner}/{repo}/{workspace_id}`
- Persistence: `completed.json`, `failed.json` with atomic writes
- tokio semaphore for `[runtime].max_concurrent`
- Single consumer loop (no races on dedup check)

### Deliverables
- [ ] `src/dispatcher.rs` — consumer loop, dedup logic, semaphore
- [ ] Atomic file writes (write to `.tmp`, rename)
- [ ] Unit tests: duplicate events skipped, different keys run, semaphore limits concurrency

### Exit Criteria
- Same event key received twice: second is skipped
- `max_concurrent = 2`: at most 2 workflows run simultaneously
- `completed.json` persists across restarts
- Dispatcher runs as single tokio task

---

## Phase 4: Workflow Runner

**Goal:** Execute multi-step workflows with git ops and Hermes API calls

### Scope
- Git clone/pull via `git2` crate
- Worktree creation/removal (`git worktree add/remove`)
- Branch naming: `ao/<sanitized-label>-<unix-timestamp>`
- Step loop: render template → call Hermes → extract response
- Template rendering with event + global variables
- Pre/post hooks: `file_not_empty`, `file_contains`
- Log files: `XX_<name>.log`, `XX_<name>.prompt`

### Deliverables
- [ ] `src/runner.rs` — workflow execution loop
- [ ] `src/git.rs` — clone, pull, worktree management, auth via `RemoteCallbacks`
- [ ] `src/hooks.rs` — hook enum + `run_hook()` dispatcher
- [ ] `src/harness.rs` — Hermes API client (`POST /v1/responses`)
- [ ] Response parsing: extract `output[].content[].type == "output_text"`
- [ ] Integration tests: full workflow with mock Hermes

### Exit Criteria
- Git clone works with token auth
- Worktree created per event, cleaned up after
- Each step renders template and calls Hermes API
- `.log` files contain full HTTP exchange + extracted message
- `.prompt` files contain rendered prompt
- Hooks validate file conditions before/after steps

---

## Phase 5: Hot-Reload & Graceful Shutdown

**Goal:** Reload workflow files on change, handle SIGINT/SIGTERM, drain active workflows, persist state

### Scope
- Workflow file watcher (notify crate)
- Hot-reload: re-parse workflows, validate, swap in-memory structs
- Signal handler task (SIGINT/SIGTERM)
- watch channel to signal shutdown
- HTTP server stops accepting new connections
- Dispatcher stops consuming from channel
- Active workflow runners drain to completion (bounded timeout)
- State persistence before exit
- Second signal: immediate `process::exit(1)`

### Deliverables
- [ ] File watcher integration
- [ ] Hot-reload logic with validation
- [ ] Signal handler in `src/main.rs`
- [ ] watch channel integration with dispatcher
- [ ] Drain logic with timeout
- [ ] Integration test: shutdown during active workflow

### Exit Criteria
- Editing a workflow `.toml` file reloads without restart
- First signal: drains active workflows, persists state, exits cleanly
- Second signal: immediate exit
- No data loss on shutdown

---

## Phase 6: Webhooks CLI

**Goal:** Manage platform webhooks via CLI subcommands

### Scope
- `yoke webhooks add` — configure platform webhooks
- `yoke webhooks remove` — delete platform webhooks
- `yoke webhooks list` — verify webhook configuration
- Minimal event subscriptions based on loaded triggers

### Deliverables
- [ ] CLI subcommands in `src/main.rs`
- [ ] Platform API clients for webhook management

### Exit Criteria
- `webhooks add` configures platform to send events to Yoke URL
- `webhooks remove` deletes Yoke webhooks from platform
- Only subscribed event types are enabled (minimal noise)

---

## Module Build Order

1. `src/config.rs` + `src/workflow.rs` + `src/template.rs` (Phase 1)
2. `src/server.rs` + `src/webhook/*` (Phase 2)
3. `src/dispatcher.rs` (Phase 3)
4. `src/runner.rs` + `src/git.rs` + `src/hooks.rs` + `src/harness.rs` (Phase 4)
5. File watcher + Signal handling in `src/main.rs` (Phase 5)
6. CLI subcommands (Phase 6)

---

## Test Strategy

See **Section 19: Testing** in the Architecture Design for detailed test cases. Summary:

| Level | Coverage |
|---|---|
| Unit | Template rendering, webhook verification, dedup logic, hooks |
| Integration | Full webhook → workflow → Hermes flow with mock server |
| E2E | Real fixture payloads from GitHub/GitLab delivery logs |

Fixture files in `tests/fixtures/` with real webhook payloads (redacted).

---

## Out of Scope

- Multi-tenant isolation
- Custom agent protocols beyond Hermes `/v1/responses`
- Workflow versioning/rollback
- UI dashboard (CLI-first approach)
- "Catch-up" mode for missed webhook deliveries
