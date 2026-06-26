FROM rust:1.94-bookworm AS chef

RUN cargo install cargo-chef --locked --version ^0.1

WORKDIR /app

FROM chef AS planner

COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

COPY . .
RUN cargo build --release --bin yoke

# ── runtime stage ─────────────────────────────────────────────
FROM debian:bookworm-slim AS runtime

RUN apt-get update && apt-get install -y --no-install-recommends \
        openssl \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/yoke /usr/local/bin/yoke

EXPOSE 8644

ENTRYPOINT ["yoke"]