# Yoke — Agent Notes

## Project Structure

```
src/
  main.rs    — CLI entrypoint (loads config, starts server)
  config.rs  — Configuration parsing, validation, and error types
```

## Key Design Decisions

- **Single binary**: Yoke is a single binary daemon. No separate library crate yet.
- **Fail-fast on startup**: Invalid config is a hard exit. Errors produce clear messages.
- **Tilde expansion**: `~` in `workdir` is expanded at load time via `shellexpand`.
- **Serde-driven validation**: Required fields are enforced by serde (missing fields = error). Semantic validation (duplicate agents, URL schemes) is done in `Config::validate()`.
- **`ConfigError` enum**: Typed errors (Io, Parse, Validation, ShellExpand) with `Display` and `Error` impls.

## Dependencies

| Crate | Purpose |
|---|---|
| `serde` | Deserialize/serialize config structs |
| `toml` | Parse config.toml files |
| `url` | Parse and validate URLs in agent config |
| `shellexpand` | Expand `~` in workdir paths |

## Running Tests

```bash
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```