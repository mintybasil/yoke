# Yoke

A Rust daemon that receives webhook events from GitHub or GitLab and runs multi-step agent workflows through the Hermes Agent REST API.

## Quick Start

This walkthrough shows a complete setup from zero to a running Yoke instance with webhooks configured on GitHub.

### 1. Create a config file

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
```

### 2. Create a workflow

Create a `.toml` file in your workflows directory (default: `./workflows`), see [yoke-workflows](https://github.com/mintybasil/yoke-workflows) for sample workflows:

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "your-username"
allowed_users = ["your-username"]

[git]
clone = true
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

### 3. Set environment variables

```bash
export HERMES_API_KEY="***"
export WEBHOOK_SECRET="***"
export GITHUB_TOKEN="***"
```

### 4. Register webhooks on your repositories

```bash
yoke --config config.toml webhooks add --workflows .
```

This reads your workflow trigger definitions and creates (or updates) the appropriate webhooks on each repository. The operation is idempotent: running it again will update existing webhooks rather than create duplicates.

Verify the webhooks were created:

```bash
yoke --config config.toml webhooks list
```

### 5. Run Yoke

```bash
cargo run -- --config /path/to/config.toml --workflows /path/to/workflows
```

Yoke listens for webhook events on `http://{host}:{port}/webhook`. The `webhook_host` setting determines the hostname used in webhook registration URLs, which may differ from the bind address (`host`), for example, binding to `0.0.0.0` locally while advertising `yoke.example.com` in webhook URLs. 

## Configuration

Yoke reads configuration from a `config.toml` file. The default path is `config.toml` in the current directory; override with `--config`.

### config.toml

| Field | Required | Description |
|---|---|---|
| `platform` | Yes | `"github"` or `"gitlab"` |
| `repos` | No (default: `[]`) | Repositories to monitor: `[{owner = "...", repo = "..."}]` |
| `gitlab_url` | No | Top-level convenience override for `gitlab.gitlab_url` (GitLab only) |

**`[[agents]]`**: at least one required, each with a unique `name`:

| Field | Required | Description |
|---|---|---|
| `name` | Yes | Agent name referenced in workflow steps |
| `base_url` | Yes | Hermes API URL, e.g. `http://localhost:8000` |

**`[runtime]`**: all fields optional:

| Field | Required | Description |
|---|---|---|
| `max_concurrent` | No (default: `0`) | Maximum concurrent workflows (`0` = unlimited) |
| `workdir` | No (default: `"~/.yoke"`) | Runtime data directory (supports `~` expansion) |
| `drain_timeout_secs` | No (default: `30`) | Seconds to wait for in-flight workflows on shutdown |

**`[server]`**:

| Field | Required | Description |
|---|---|---|
| `host` | No (default: `"0.0.0.0"`) | Server bind address |
| `port` | No (default: `8644`) | Server listen port |
| `webhook_host` | Yes | External hostname for webhook registration URLs |
| `max_body_size` | No (default: `1048576`) | Maximum request body size in bytes |
| `catch_up_enabled` | No (default: `true`) | Replay missed webhook events on startup |
| `catch_up_max_age_hours` | No (default: `24`) | Maximum age of events to replay (in hours) |

**`[gitlab]`**: only when `platform = "gitlab"`:

| Field | Required | Description |
|---|---|---|
| `gitlab_url` | No (default: `"https://gitlab.com"`) | GitLab instance URL (for self-hosted GitLab) |

`server.webhook_host` must be explicitly set: it is the hostname that GitHub/GitLab will send webhook events to, which typically differs from the bind address (`server.host`).

> **Limitations:** GitHub returns at most 100 recent deliveries per webhook. GitLab returns up to 100 events per page. Catch-up runs before the HTTP listener starts, so large backlogs may delay server readiness.

### Environment Variables

| Variable | Purpose | Required |
|---|---|---|
| `HERMES_API_KEY` | Bearer token for Hermes REST API | Always |
| `WEBHOOK_SECRET` | Webhook authentication key (GitHub HMAC key or GitLab token) | Always |
| `GITHUB_TOKEN` | GitHub auth for webhook management and git operations | When `platform = "github"` |
| `GITLAB_TOKEN` | GitLab auth for webhook management and git operations | When `platform = "gitlab"` |

### Token Permissions

The tokens used for webhook management must have the correct permissions/scopes, otherwise the GitHub or GitLab API will return 404 (GitHub) or 401/403 (GitLab) even if the repository exists.

**GitHub Classic Token (Personal Access Token):**

- `repo` (full repository access), required for cloning/pushing
- `admin:repo_hook` (read/write), required for webhook management
- Or simply enable the full `repo` scope which includes `admin:repo_hook`

**GitHub Fine-grained Token:**

- **Repository permissions → Administration**: Read and Write, required for webhook management
- **Repository permissions → Contents**: Read, required for git operations
- Note: Fine-grained tokens use `Bearer` authentication (which Yoke now sends). Using a fine-grained token without the Administration permission will cause 404 responses on the webhooks endpoints.

**GitLab Token:**

- `api` scope, required for all webhook management and git operations

## Workflows

Workflows are defined in `.toml` files in a directory. Each file specifies a trigger, optional git configuration, and a sequence of steps to execute.

> [!NOTE]
> Sample workflows are available in [mintybasil/yoke-workflows](https://github.com/mintybasil/yoke-workflows).

### Example workflow

```toml
[trigger]
type = "github_issue_assigned"
assigned_to = "alice"
allowed_users = ["alice"]

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

| Field | Purpose | Required |
|---|---|---|
| `[trigger].type` | Event type (e.g. `github_issue_assigned`) | Yes |
| `[trigger].allowed_users` | Users permitted to trigger this workflow | Yes |
| `[git].clone` | Whether to git clone the repo | No (default: `true`) |
| `[git].default_branch` | Branch for clone/worktree base | No (default: `"main"`) |
| `[[steps]].name` | Human-readable step label | Yes |
| `[[steps]].agent` | Agent name from `config.toml` | Yes |
| `[[steps]].prompt_template` | `{{variable}}` template rendered at runtime | Yes |
| `[[steps]].pre_hooks` | Hooks to check before step | No (default: none) |
| `[[steps]].post_hooks` | Hooks to check after step | No (default: none) |

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

Additional variables are available depending on the trigger type (see the trigger tables below). The [Architecture Design](docs/Architecture%20Design.md#appendix-a-trigger-reference) doc has the full reference including event ID formats and actor sources.

### Trigger types

Triggers are platform-specific and must match the `platform` setting in `config.toml`.

**GitHub triggers** (`platform = "github"`):

| Trigger | Event | Variables | Event Filter |
|---|---|---|---|
| `github_issue_assigned` | Issue assigned to a user | `issue_number`, `assignee`, `issue_title`, `issue_body` | `assigned_to` |
| `github_issue_comment_mention` | Comment on an issue mentions a user | `issue_number`, `comment_id`, `comment_body`, `mentioned_user` | `mentioned_user` |
| `github_pull_request_review` | Pull request review submitted | `pr_number`, `review_id`, `review_body` | none |
| `github_pull_request_comment_mention` | Pull request review comment | `pr_number`, `comment_id`, `comment_body`, `mentioned_user` | `mentioned_user` |

**GitLab triggers** (`platform = "gitlab"`):

| Trigger | Event | Variables | Event Filter |
|---|---|---|---|
| `gitlab_issue_assigned` | Issue assigned to a user | `issue_iid`, `action`, `assignee_username`, `issue_title`, `issue_body` | `assigned_to` |
| `gitlab_issue_mention` | Note on an issue mentions a user | `issue_iid`, `note_id`, `comment_body` | `mentioned_user` |
| `gitlab_merge_request_review` | Note on a merge request | `mr_iid`, `review_id`, `review_body` | none |
| `gitlab_merge_request_comment_mention` | DiffNote on a merge request | `mr_iid`, `note_id`, `comment_body` | `mentioned_user` |

### Hot-reload

Yoke watches the `--workflows` directory and automatically reloads `.toml` files on change: no restart required. Validation errors during reload are logged and the previous workflow state is preserved.

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

Creates or updates webhooks on all configured repositories, subscribing to the event types derived from your workflow triggers. The operation is idempotent: existing webhooks matching the Yoke URL are updated; new ones are created.

**Remove webhooks:**

```bash
yoke --config config.toml webhooks remove
```

Removes all Yoke webhooks (matched by URL) from each configured repository.

## Secure Webhook Exposure with Tailscale Funnel

If you are running Yoke locally but need to receive webhooks from GitHub or GitLab, you can use [**Tailscale Funnel**](https://tailscale.com/docs/features/tailscale-funnel) to securely expose your local server to the internet without configuring firewall rules or port forwarding.

### Overview

Tailscale Funnel routes public internet traffic to a port on your machine over your Tailscale network. You advertise a public DNS name that GitHub or GitLab can send webhook events to, and Funnel forwards that traffic to your local Yoke instance.

### Setup Steps

1. **Ensure Funnel is enabled:** In the [Tailscale Admin Console](https://login.tailscale.com/admin/machines), enable Funnel for your node.
2. **Expose the Yoke port:** Run the following command to expose Yoke's default port:
   ```bash
   tailscale funnel 8644
   ```
3. **Configure Yoke:** Use your Tailscale DNS name (e.g., `machine.tailnet-name.ts.net`) as the `webhook_host` in `config.toml`:
   ```toml
   [server]
   webhook_host = "machine.tailnet-name.ts.net"
   ```

For detailed configuration and advanced options, refer to the [official Tailscale Funnel documentation](https://tailscale.com/docs/features/tailscale-funnel).

## Further Reading

- [Architecture Design](docs/Architecture%20Design.md): internal design, data flow, and full trigger variable reference
