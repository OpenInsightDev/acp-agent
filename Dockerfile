# syntax=docker/dockerfile:1.7

# The context carries the release binary under `linux/<arch>/`, where <arch> is
# the amd64 or arm64 that BuildKit resolves TARGETARCH to. Nothing is compiled
# here, so no architecture needs emulation.

FROM scratch AS bin

ARG TARGETARCH
COPY linux/$TARGETARCH/acp-agent /acp-agent
# `docker create` takes its command from the image, and a scratch one brings none.
CMD ["/acp-agent"]

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
