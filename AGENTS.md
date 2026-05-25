# Yoke — Agent Notes

## Project Structure

```
src/
  main.rs      — CLI entrypoint (loads config, starts server)
  config.rs    — Configuration parsing, validation, and error types
  workflow.rs  — Workflow TOML parsing, validation, and error types
  template.rs  — Template rendering with `{{variable}}` substitution and validation
```

## Key Design Decisions

- **Single binary**: Yoke is a single binary daemon. No separate library crate yet.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes, trigger types) is done in `Config::validate()` / `Workflow::validate()`.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand, AgentResolution) with `Display` and `Error` impls.
- **Agent resolution**: `resolve_agents(config, workflows)` validates that every `step.agent` in every workflow matches a configured `[[agents]]` name. Returns `ConfigError::AgentResolution` with step name, workflow path, and missing agent.
- **`WorkflowError` enum**: Typed errors (Io, Parse, Validation) with `Display` and `Error` impls. Parse/Validation errors include the file path for clear diagnostics.
- **`Workflow.path` field**: Each `Workflow` carries its source file path (populated by `load_workflows`), used for agent resolution error reporting.
- **Template renderer**: `template::render()` does `{{var}}` substitution, returning `Result<_, TemplateError>` for unknown variables, malformed syntax, and empty templates.

## Dependencies

| Crate | Purpose |
|---|---|
| `serde` | Deserialize/serialize config and workflow structs |
| `toml` | Parse config.toml and workflow .toml files |
| `url` | Parse and validate URLs in agent config |
| `shellexpand` | Expand `~` in workdir paths |

## Running Tests

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```