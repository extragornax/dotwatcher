# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
WORKDIR /app

# Prime the dependency cache: build an empty crate with the real manifests first
# so `cargo build --release` only rebuilds our source on subsequent edits.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && \
    rm -rf src target/release/madcap_fast target/release/deps/madcap_fast*

COPY src ./src
# Bust cargo's mtime-based cache so main.rs is recompiled.
RUN touch src/main.rs && cargo build --release


FROM debian:bookworm-slim AS runtime
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl tini && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --no-create-home --uid 10001 madcap
USER madcap

COPY --from=builder /app/target/release/madcap_fast /usr/local/bin/madcap_fast

ENV PORT=8080 \
    RUST_LOG=madcap_fast=info,tower_http=info \
    MADCAP_WARM_SLUG=desertus-bikus-26

EXPOSE 8080

HEALTHCHECK --interval=30s --timeout=3s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT}/" >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/madcap_fast"]
