# Implementation Plan

This document outlines the phased approach for implementing the yoke agent orchestrator. It complements the [Architecture Design](./Architecture%20Design.md) by focusing on execution order, milestones, and delivery structure rather than architectural details.

## Guiding Principles

1. **Vertical slices over horizontal layers** — Each phase delivers end-to-end functionality for a subset of features
2. **Config-driven first** — Hardcoded paths become configuration before becoming dynamic APIs
3. **Single-platform MVP** — Launch with one platform provider before abstracting
4. **Observability from day one** — Tracing, metrics, and structured logging in every phase

---

## Phase 0: Foundation

**Goal:** Establish the core runtime skeleton with zero external dependencies

### Scope
- Project scaffolding (Rust workspace, Cargo.toml structure)
- Core types: `Job`, `Step`, `Agent`, `Platform`
- Configuration loading from `config.toml`
- Basic tracing infrastructure (tracing-subscriber)
- Unit test harness

### Deliverables
- [ ] `cargo init` workspace with `yoke-core`, `yoke-config`, `yoke-runtime` crates
- [ ] Config parsing with serde + toml
- [ ] Type definitions matching architecture doc
- [ ] `tracing` wired to stdout with JSON formatter
- [ ] CI: `cargo check`, `cargo test` on push

### Exit Criteria
- Config file parses successfully
- All core types compile with zero warnings
- Tests pass locally and in CI

**Duration:** 1-2 days

---

## Phase 1: Single-Platform Execution

**Goal:** Run a complete workflow against one platform (GitHub) with hardcoded paths

### Scope
- GitHub platform provider implementation
- Workflow parser (`.toml` workflow files)
- Step executor with sequential execution
- Webhook receiver (single `/webhook` endpoint)
- Deduplication layer (in-memory cache)

### Deliverables
- [ ] `yoke-platform-github` crate with:
  - PR comment trigger parsing
  - Status check updates
  - Commit status API integration
- [ ] Workflow TOML parser with validation
- [ ] Step runner: resolve agent → dispatch → poll → complete
- [ ] Axum-based HTTP server with `/webhook` route
- [ ] Dedup cache: `{owner}/{repo}/{workflow_id}` → timestamp
- [ ] Integration test: full workflow execution against test repo

### Exit Criteria
- Comment `@yoke run deploy` on a PR triggers workflow
- Workflow executes all steps sequentially
- Dedup prevents duplicate runs within 5-minute window
- All events logged with trace IDs

**Duration:** 3-5 days

---

## Phase 2: Configuration & Multi-Platform

**Goal:** Abstract platform layer, support multiple providers via config

### Scope
- Platform trait abstraction
- Second platform provider (GitLab or custom HTTP)
- Config schema evolution (multi-platform support)
- Agent routing by step configuration

### Deliverables
- [ ] `Platform` trait with provider-agnostic interface
- [ ] Refactor GitHub provider to implement trait
- [ ] Second provider implementation (choice based on user priority)
- [ ] Config schema: `[[platforms]]` array
- [ ] Step-level `platform` field routing
- [ ] Feature flags for platform providers

### Exit Criteria
- Workflows can specify different platforms per step
- Adding a new platform requires only config changes
- No regression in Phase 1 functionality

**Duration:** 2-3 days

---

## Phase 3: Persistence & Reliability

**Goal:** Production-hardened state management and failure recovery

### Scope
- SQLite persistence layer (job state, step results)
- Retry logic with exponential backoff
- Dead letter queue for failed steps
- Graceful shutdown and signal handling

### Deliverables
- [ ] `yoke-store` crate with SQLite backend (sqlx)
- [ ] Job state machine: `Pending → Running → Completed/Failed`
- [ ] Retry policy: configurable attempts + backoff
- [ ] DLQ: failed steps written for manual inspection
- [ ] Signal handlers: SIGTERM drains in-flight jobs
- [ ] Migration system for schema evolution

### Exit Criteria
- Restart preserves in-flight job state
- Failed steps retry 3 times before DLQ
- Graceful shutdown completes within 30s
- Schema migrations run on startup

**Duration:** 3-4 days

---

## Phase 4: Observability & Operations

**Goal:** Production-ready monitoring, debugging, and operational tooling

### Scope
- Metrics export (Prometheus)
- Distributed tracing (OpenTelemetry)
- Admin CLI for inspection
- Health check endpoints

### Deliverables
- [ ] Metrics: job duration, step latency, queue depth, error rates
- [ ] OpenTelemetry traces exported to Jaeger/Tempo
- [ ] `yoke-cli` with:
  - `yoke status` — running jobs
  - `yoke inspect <job-id>` — step details
  - `yoke retry <job-id>` — manual retry from DLQ
- [ ] `/health` and `/ready` endpoints
- [ ] Structured log correlation with trace IDs

### Exit Criteria
- Grafana dashboard shows real-time job metrics
- Traces visible in Tempo/Jaeger with full step breakdown
- CLI can inspect and retry any job
- Kubernetes-style health checks pass

**Duration:** 2-3 days

---

## Phase 5: Advanced Features

**Goal:** Feature parity with agent-orchestrator vision

### Scope
- Parallel step execution
- Conditional branching in workflows
- Secret management
- Workflow templates

### Deliverables
- [ ] Parallel execution: `run_in_parallel` step group
- [ ] Conditionals: `if` expressions on steps
- [ ] Secrets: encrypted storage + injection
- [ ] Template system: parameterized workflows

### Exit Criteria
- Workflows can express complex DAGs
- Secrets never appear in logs
- Templates reduce workflow duplication

**Duration:** 4-6 days

---

## Risk Mitigation

| Risk | Mitigation |
|------|------------|
| Platform API rate limits | Implement request queuing + backoff in Phase 1 |
| State corruption on crash | SQLite WAL mode + transactional writes in Phase 3 |
| Webhook delivery gaps | Provider-side retry + idempotency keys |
| Config schema drift | Version field in config + migration path |

---

## Dependencies & Prerequisites

- Rust 1.75+ (for async trait improvements)
- SQLite 3.35+ (for JSON functions if needed)
- Platform API access (GitHub tokens, etc.)
- Observability stack (Prometheus, Tempo) for Phase 4

---

## Out of Scope (Future Phases)

- Multi-tenant isolation
- Custom agent protocols beyond HTTP
- Workflow versioning/rollback
- UI dashboard (CLI-first approach)

---

## Success Metrics

By end of Phase 3:
- 99% of webhooks processed within 2 seconds
- Zero data loss on process restart
- Mean time to recovery < 5 minutes

By end of Phase 5:
- Support for 2+ platforms with identical workflow syntax
- < 1% of jobs require manual intervention
- Full traceability from webhook to final step
