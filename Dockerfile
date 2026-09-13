# syntax=docker/dockerfile:1.7

FROM rust:1.98-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY data ./data

RUN cargo build --release --locked \
    && install -Dm755 target/release/acp-agent /out/acp-agent

FROM scratch AS bin

COPY --from=builder /out/acp-agent /acp-agent

FROM debian:bookworm AS latest

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        libsqlite3-0 \
        libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=bin /acp-agent /acp-agent
COPY --from=ghcr.io/denoland/deno:bin /deno /usr/local/bin/deno
COPY --from=ghcr.io/astral-sh/uv:latest /uv /uvx /bin/

ENV HOME=/root \
    XDG_CACHE_HOME=/cache \
    DENO_INSTALL_ROOT=/root/.deno \
    PATH=/root/.deno/bin:/root/.local/bin:/usr/local/bin:/usr/bin:/bin \
    DENO_NO_UPDATE_CHECK=1 \
    UV_NO_PROGRESS=1

WORKDIR /workspace

ENTRYPOINT ["/acp-agent"]
CMD ["--help"]
