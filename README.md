# Yoke

A Rust daemon that receives webhook events from GitHub or GitLab and runs multi-step agent workflows through the Hermes Agent REST API.

## Quick Start

This walkthrough shows a complete setup from zero to a running Yoke instance with webhooks configured on GitHub.

### 1. Build

```bash
cargo build
```

### 2. Create a config file

Create `config.toml` in your project directory:

```toml
platform = "github"

repos = [
    { owner = "your-org", repo = "your-repo" },
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
webhook_host = "yoke.example.com"
webhook_secret = "your-webhook-secret"
```

### 3. Create a workflow

Create a `.toml` file in your workflows directory (default: `./workflows`):

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "your-username"

[git]
clone = true
worktree = true
default_branch = "main"

[[steps]]
name = "Plan"
agent = "pm"
prompt_template = "Plan the issue: {{issue_title}}"

[[steps]]
name = "Implement"
agent = "swe"
prompt_template = "Implement the plan in plan.md"
post_hooks = [{ type = "file_not_empty", path = "plan.md" }]
```

### 4. Set environment variables

```bash
export HERMES_API_KEY="your-hermes-api-key"
export WEBHOOK_SECRET="your-webhook-secret"
export GITHUB_TOKEN="your-github-token"
```

### 5. Register webhooks on your repositories

```bash
yoke --config config.toml webhooks add --workflows .
```

This reads your workflow trigger definitions and creates (or updates) the appropriate webhooks on each repository. The operation is idempotent — running it again will update existing webhooks rather than create duplicates.

Verify the webhooks were created:

```bash
yoke --config config.toml webhooks list
```

### 6. Run Yoke

```bash
# Run with defaults (loads config.toml from current directory)
cargo run

# Or specify config and workflows paths
cargo run -- --config /path/to/config.toml --workflows /path/to/workflows
```

Yoke listens for webhook events on `http://{host}:{port}/webhook`. The `webhook_host` setting determines the hostname used in webhook registration URLs, which may differ from the bind address (`host`) — for example, binding to `0.0.0.0` locally while advertising `yoke.example.com` in webhook URLs. 

## Configuration

Yoke reads configuration from a `config.toml` file. The default path is `config.toml` in the current directory; override with `--config`.

### config.toml

```toml
# Platform: "github" or "gitlab"
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
drain_timeout_secs = 30  # seconds to wait for in-flight workflows on shutdown

# Server settings
[server]
host = "0.0.0.0"
port = 8644
webhook_host = "yoke.example.com"
webhook_secret = "your-webhook-secret"
max_body_size = 1048576   # 1MB default

# GitLab-specific (only when platform = "gitlab")
# gitlab_url = "https://gitlab.mycompany.com"
```

### Required Fields

- `platform` — must be `"github"` or `"gitlab"`
- `agents` — at least one agent with a unique `name` and valid `base_url`
- `server.webhook_secret` — webhook authentication key
- `server.webhook_host` — external hostname used in webhook registration URLs. This must be explicitly set (e.g., `yoke.example.com`) — it is the hostname that GitHub/GitLab will send webhook events to, which typically differs from the bind address (`server.host`).

### Environment Variables

| Variable | Purpose | Required |
|---|---|---|
| `HERMES_API_KEY` | Bearer token for Hermes REST API | Always |
| `WEBHOOK_SECRET` | Webhook authentication key (overrides `server.webhook_secret` in config) | No (config fallback) |
| `GITHUB_TOKEN` | GitHub auth for webhook management and git operations | When `platform = "github"` |
| `GITLAB_TOKEN` | GitLab auth for webhook management and git operations | When `platform = "gitlab"` |

### Token Permissions

The tokens used for webhook management must have the correct permissions/scopes, otherwise the GitHub or GitLab API will return 404 (GitHub) or 401/403 (GitLab) even if the repository exists.

**GitHub Classic Token (Personal Access Token):**

- `repo` (full repository access) — required for cloning/pushing
- `admin:repo_hook` (read/write) — required for webhook management
- Or simply enable the full `repo` scope which includes `admin:repo_hook`

**GitHub Fine-grained Token:**

- **Repository permissions → Administration**: Read and Write — required for webhook management
- **Repository permissions → Contents**: Read — required for git operations
- Note: Fine-grained tokens use `Bearer` authentication (which Yoke now sends). Using a fine-grained token without the Administration permission will cause 404 responses on the webhooks endpoints.

**GitLab Token:**

- `api` scope — required for all webhook management and git operations

## Webhook Management

The `webhooks` subcommand provides a unified CLI for managing repository webhooks across GitHub and GitLab. It reads the `platform` and `repos` settings from `config.toml` and authenticates using `GITHUB_TOKEN` or `GITLAB_TOKEN`.

**List webhooks:**

```bash
yoke --config config.toml webhooks list
```

Lists all webhooks for each repository in `config.toml`, including ID, URL, events, active status, and redacted secret.

**Add webhooks:**

```bash
yoke --config config.toml webhooks add [--workflows <DIR>]
```

Creates or updates webhooks on all configured repositories, subscribing to the event types derived from your workflow triggers. The operation is idempotent — existing webhooks matching the Yoke URL are updated; new ones are created.

**Remove webhooks:**

```bash
yoke --config config.toml webhooks remove
```

Removes all Yoke webhooks (matched by URL) from each configured repository.

## Workflows

Workflows are defined in `.toml` files in a directory (default: `./workflows`; override with `--workflows`). Each file specifies a trigger, optional git configuration, and a sequence of steps to execute.

### Example workflow

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

### Workflow fields

| Field | Purpose | Default |
|---|---|---|
| `[trigger].type` | Event type (e.g. `github_issue_assigned`) | required |
| `[trigger].assigned_to` | Event-content filter: only fire when the issue is assigned to this user | optional |
| `[trigger].mentioned_user` | Event-content filter: only fire when this user is @mentioned | optional |
| `[trigger].allowed_users` | **SECURITY BOUNDARY**: which usernames are permitted to trigger this workflow (checks the actor, NOT the assignee/mentioned user) | required |
| `[git].clone` | Whether to git clone the repo | `true` |
| `[git].worktree` | Whether to create a per-event worktree | `true` |
| `[git].default_branch` | Branch for clone/worktree base | `"main"` |
| `[[steps]].name` | Human-readable step label | required |
| `[[steps]].agent` | Agent name from `config.toml` | required |
| `[[steps]].prompt_template` | `{{variable}}` template rendered at runtime | required |
| `[[steps]].pre_hooks` | Hooks to check before step | none |
| `[[steps]].post_hooks` | Hooks to check after step | none |

### Hooks

Hooks validate file conditions before and after each step. A hook failure stops the workflow.

Hook `path` and `text` fields support `{{variable}}` template syntax, the same variables available in `prompt_template`. Template variables are resolved before the hook runs, so you can reference dynamic paths like `{{output_dir}}/plan.md`.

```toml
pre_hooks = [{ type = "file_not_empty", path = "{{output_dir}}/plan.md" }]
post_hooks = [{ type = "file_contains", path = "plan.md", text = "implementation" }]
```

| Hook | Fields | Description |
|---|---|---|
| `file_not_empty` | `path` | Checks that a file exists and has non-zero content |
| `file_contains` | `path`, `text` | Checks that a file contains a specific string |

### Template variables

Step `prompt_template` fields use `{{variable}}` syntax. The following variables are available in all triggers:

| Variable | Value |
|---|---|
| `owner` | Repository owner (namespace) |
| `repo` | Repository name |
| `output_dir` | Per-event workspace directory |
| `event_id` | Unique event identifier for deduplication |
| `repo_path` | Full repository path (`owner/repo`) |

Additional variables are available depending on the trigger type. See the [Architecture Design](docs/Architecture%20Design.md#appendix-a-trigger-reference) doc for the full trigger reference including all filters and event ID formats.

### Trigger types

Triggers are platform-specific and must match the `platform` setting in `config.toml`.

**GitHub triggers** (`platform = "github"`):

| Trigger | Event | Variables |
|---|---|---|
| `github_issue_assigned` | Issue assigned to a user | `issue_number`, `assignee`, `issue_title`, `issue_body` |
| `github_issue_comment_mention` | Comment on an issue mentions a user | `issue_number`, `comment_id`, `comment_body` |
| `github_pull_request_review` | Pull request review submitted | `pr_number`, `review_id`, `review_body` |
| `github_pull_request_comment_mention` | Pull request review comment | `pr_number`, `review_id`, `comment_id`, `comment_body` |

**GitLab triggers** (`platform = "gitlab"`):

| Trigger | Event | Variables |
|---|---|---|
| `gitlab_issue_assigned` | Issue assigned to a user | `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body` |
| `gitlab_issue_mention` | Note on an issue mentions a user | `issue_iid`, `note_id`, `comment_body` |
| `gitlab_merge_request_review` | Note on a merge request | `mr_iid`, `review_id`, `review_body` |
| `gitlab_merge_request_comment_mention` | DiffNote on a merge request | `mr_iid`, `note_id`, `comment_body` |

### Hot-reload

Yoke watches the `--workflows` directory and automatically reloads `.toml` files on change — no restart required. Validation errors during reload are logged and the previous workflow state is preserved.

## CLI Reference

```
yoke [OPTIONS]
yoke webhooks <SUBCOMMAND>

Options:
  --config <PATH>       Path to config.toml (default: config.toml)
  --workflows <DIR>      Directory containing workflow TOML files (default: ./workflows)
  --host <ADDR>          Server bind address (overrides config.toml)
  --port <PORT>          Server listen port (overrides config.toml)
  --webhook-host <HOST>  External hostname for webhook URLs (overrides config.toml webhook_host)
  -h, --help             Print help
  -V, --version          Print version

Webhook subcommands:
  webhooks list              List webhooks for all configured repositories
  webhooks add               Add or update webhooks based on workflow triggers
  webhooks remove            Remove Yoke webhooks from all configured repositories
```

## Further Reading

- [Architecture Design](docs/Architecture%20Design.md) — internal design, data flow, and full trigger variable reference