# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder
WORKDIR /app

# Prime the dependency cache: build an empty crate with the real manifests first
# so `cargo build --release` only rebuilds our source on subsequent edits.
# `benches/merge_tracks.rs` only needs to exist for Cargo.toml to parse; the
# stub stays in place since `cargo build` never compiles benches.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src benches && \
    echo 'fn main() {}' > src/main.rs && \
    : > src/lib.rs && \
    echo 'fn main() {}' > benches/merge_tracks.rs && \
    cargo build --release && \
    rm -rf src target/release/madcap_fast target/release/deps/madcap_fast*

COPY src ./src
# Bust cargo's mtime-based cache so sources are recompiled.
RUN touch src/main.rs src/lib.rs && cargo build --release


FROM debian:bookworm-slim AS runtime
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates curl tini && \
    rm -rf /var/lib/apt/lists/*

RUN useradd --system --no-create-home --uid 10001 madcap
USER madcap

COPY --from=builder /app/target/release/madcap_fast /usr/local/bin/madcap_fast

ENV PORT=9004 \
    RUST_LOG=madcap_fast=info,tower_http=info \
    MADCAP_WARM_SLUG=desertus-bikus-26

EXPOSE 9004

HEALTHCHECK --interval=30s --timeout=3s --start-period=20s --retries=3 \
    CMD curl -fsS "http://127.0.0.1:${PORT}/" >/dev/null || exit 1

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/madcap_fast"]
