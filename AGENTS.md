# Yoke — Agent Notes

## Project Structure

```
src/
  main.rs      — CLI entrypoint (loads config, starts server)
  config.rs    — Configuration parsing, validation, and error types
  workflow.rs  — Workflow TOML parsing, validation, and error types
  template.rs  — Template rendering with `{{variable}}` and `{{{variable}}}` substitution
```

## Key Design Decisions

- **Single binary**: Yoke is a single binary daemon. No separate library crate yet.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes, trigger types) is done in `Config::validate()` / `Workflow::validate()`.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand) with `Display` and `Error` impls.
- **`WorkflowError` enum**: Typed errors (Io, Parse, Validation) with `Display` and `Error` impls. Parse/Validation errors include the file path for clear diagnostics.
- **Template renderer**: `template::render()` does `{{var}}` and `{{{var}}}` substitution with fail-fast validation. Unknown variables, malformed syntax, and empty templates all panic with clear error messages.

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