# syntax=docker/dockerfile:1

FROM rust:1.88 AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /app/target/release/codex-proxy-rs /usr/local/bin/codex-proxy-rs

EXPOSE 8080
CMD ["codex-proxy-rs", "--config", "config.yaml"]
