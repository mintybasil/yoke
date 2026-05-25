# Yoke

A Rust daemon that receives webhook events from GitHub or GitLab and runs multi-step agent workflows through the Hermes Agent REST API.

## Quick Start

```bash
# Build
cargo build

# Run tests
cargo test

# Lint
cargo fmt --check
cargo clippy -- -D warnings

# Run with a config file
cargo run -- config.toml
```

## Configuration

Yoke reads configuration from a `config.toml` file. The path is passed as the first CLI argument (defaults to `config.toml` in the current directory).

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

## Architecture

See [docs/Architecture Design.md](docs/Architecture%20Design.md) for the full system design.

## License

TBD