# syntax=docker/dockerfile:1.7

FROM rust:1.96-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked \
    && install -Dm755 target/release/acp-agent /out/acp-agent

FROM debian:bookworm-slim AS toolchain

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
    unzip \
    && rm -rf /var/lib/apt/lists/*

ENV DENO_INSTALL=/opt/deno \
    UV_INSTALL_DIR=/usr/local/bin

RUN set -eux; \
    curl -fsSL https://deno.land/install.sh | sh; \
    curl -LsSf https://astral.sh/uv/install.sh | sh; \
    test -x /opt/deno/bin/deno; \
    test -x /usr/local/bin/uv; \
    test -x /usr/local/bin/uvx; \
    mkdir -p /workspace /cache /home/nonroot; \
    chown -R 65532:65532 /workspace /cache /home/nonroot

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /out/acp-agent /usr/local/bin/acp-agent
COPY --from=toolchain /opt/deno/bin/deno /usr/local/bin/deno
COPY --from=toolchain /usr/local/bin/uv /usr/local/bin/uv
COPY --from=toolchain /usr/local/bin/uvx /usr/local/bin/uvx
COPY --from=toolchain /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=toolchain --chown=65532:65532 /workspace /workspace
COPY --from=toolchain --chown=65532:65532 /cache /cache
COPY --from=toolchain --chown=65532:65532 /home/nonroot /home/nonroot

ENV HOME=/home/nonroot \
    XDG_CACHE_HOME=/cache \
    DENO_INSTALL_ROOT=/home/nonroot/.deno \
    PATH=/home/nonroot/.deno/bin:/home/nonroot/.local/bin:/usr/local/bin:/usr/bin:/bin \
    DENO_NO_UPDATE_CHECK=1 \
    UV_NO_PROGRESS=1

USER 65532:65532
WORKDIR /workspace

ENTRYPOINT ["acp-agent"]
CMD ["--help"]
