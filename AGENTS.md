# Yoke — Agent Notes

## Project Structure

```
src/
  main.rs      — CLI entrypoint (loads config, loads workflows, validates agents & triggers, starts server)
  cli.rs       — CLI argument parsing (clap derive)
  config.rs    — Configuration parsing, validation, and error types
  server.rs    — axum HTTP server with health, readiness, and unified webhook endpoint
  workflow.rs  — Workflow TOML parsing, validation, and error types
  template.rs  — Template rendering with `{{variable}}` substitution and validation
  webhook/     — Webhook handling modules
    mod.rs       — Shared types (TriggerEvent, WebhookError) and dispatch to platform handler
    github.rs   — GitHub webhook: HMAC-SHA256 verification, event parsing, trigger mapping
    gitlab.rs   — GitLab webhook: token verification, payload parsing, event mapping
```

## Key Design Decisions

- **Single binary**: Yoke is a single binary daemon. No separate library crate yet.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **CLI argument parsing**: Uses `clap` with derive macros. `--config` and `--workflows` have defaults; `--host` and `--port` override values from `config.toml`.
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes, trigger types) is done in `Config::validate()` / `Workflow::validate()`.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand, AgentResolution, EnvVar) with `Display` and `Error` impls.
- **Agent resolution**: `resolve_agents(config, workflows)` validates that every `step.agent` in every workflow matches a configured `[[agents]]` name. Returns `ConfigError::AgentResolution` with step name, workflow path, and missing agent.
- **Environment variable validation**: `validate_env_vars(platform)` checks required env vars at startup. `HERMES_API_KEY` and `WEBHOOK_SECRET` are always required; `GITHUB_TOKEN` is required when `platform = "github"`, `GITLAB_TOKEN` when `platform = "gitlab"`. Returns `ConfigError::EnvVar` with a descriptive message.
- **`WEBHOOK_SECRET` env override**: The `WEBHOOK_SECRET` env var overrides the `config.toml` `server.webhook_secret` value at startup.
- **Trigger platform validation**: After loading config and workflows, `validate_triggers()` checks that each workflow's trigger type prefix matches the configured platform. GitLab triggers (`gitlab_*`) on a GitHub platform (and vice versa) cause a hard exit with a clear error.
- **`TriggerType` enum**: Typed representation of known trigger types (4 GitHub, 4 GitLab). Each variant carries its required filter fields per Appendix A. `TriggerType::from_trigger()` converts a `Trigger` struct; `TriggerType::platform()` returns the owning platform; `TriggerType::label()` returns the string identifier used in workflow TOML files.
- **`WorkflowError` enum**: Typed errors (Io, Parse, Validation) with `Display` and `Error` impls. Parse/Validation errors include the file path for clear diagnostics.
- **`Workflow.path` field**: Each `Workflow` carries its source file path (populated by `load_workflows`), used for agent resolution error reporting.
- **Template renderer**: `template::render()` does `{{var}}` substitution, returning `Result<_, TemplateError>` for unknown variables, malformed syntax, and empty templates.
- **HTTP server**: `src/server.rs` uses axum with `tower-http` middleware. Three endpoints: `/health` (liveness, returns `{"status":"ok"}`), `/ready` (readiness, returns 200 — always ready for now), `/webhook` (POST — dispatches to platform-specific handler based on `platform` config). `RequestBodyLimitLayer` enforces `max_body_size` from config. `TraceLayer` provides structured HTTP request logging.
- **Webhook dispatch**: The server extracts platform-appropriate headers (`X-Hub-Signature-256`/`X-GitHub-Event` for GitHub, `X-Gitlab-Token`/`X-Gitlab-Event` for GitLab) and delegates to `webhook::dispatch_webhook`, which routes to the correct handler. Both handlers return `Result<TriggerEvent, WebhookError>`.
- **GitHub webhook handler**: `src/webhook/github.rs` provides HMAC-SHA256 signature verification (`verify_github_signature`), JSON payload parsing (`parse_github_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_github_webhook` function orchestrates the full flow.
- **GitLab webhook handler**: `src/webhook/gitlab.rs` provides constant-time token verification (`verify_gitlab_token`), JSON payload parsing (`parse_gitlab_event`), and event-to-trigger mapping (`map_to_trigger_event`). The `handle_gitlab_webhook` function orchestrates the full flow.
- **Constant-time comparison**: Both handlers use `subtle::ConstantTimeEq` to prevent timing attacks — GitHub for HMAC signatures, GitLab for token comparison.
- **`WebhookError` enum**: Shared error type (Unauthorized, BadRequest, NoMatchingTrigger) in `webhook/mod.rs`, used by both platform handlers via `dispatch_webhook`.
- **`TriggerEvent` struct**: Shared webhook result type in `webhook/mod.rs` with `trigger_type: TriggerType`, `repo_path`, and `event_id` fields.

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

## Running Tests

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```