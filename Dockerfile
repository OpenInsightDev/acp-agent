# syntax=docker/dockerfile:1.7

FROM rust:1.97-bookworm AS builder

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

ARG TARGETARCH
ARG DENO_VERSION=2.9.4
ARG UV_VERSION=0.12.1

# SHA-256 values are from the official Deno and uv release metadata.
RUN set -eux; \
    case "$TARGETARCH" in \
        amd64) \
            deno_arch="x86_64"; \
            deno_sha256="c24f955d9fbfe0ea5ae2b501c8e71ae76e31e4c9782390a54a284b3364fda725"; \
            uv_sha256="90b2f223fb69d19db49e117da601f64978593417988530aa733d456141b4bcbb"; \
            ;; \
        arm64) \
            deno_arch="aarch64"; \
            deno_sha256="111da5c05c240cfdc4340f234a0e3539d39dbcb6755221f19dcd60bacc8be5aa"; \
            uv_sha256="769d373e146692c639b5fbaae33b331c297a32e03d30448772051902df52bbf4"; \
            ;; \
        *) \
            echo "unsupported target architecture: $TARGETARCH" >&2; \
            exit 1; \
            ;; \
    esac; \
    deno_archive="/tmp/deno-${DENO_VERSION}.zip"; \
    uv_archive="/tmp/uv-${UV_VERSION}.tar.gz"; \
    curl --fail --silent --show-error --location --retry 3 --retry-delay 2 \
        --connect-timeout 15 --max-time 300 \
        "https://dl.deno.land/release/v${DENO_VERSION}/deno-${deno_arch}-unknown-linux-gnu.zip" \
        -o "$deno_archive"; \
    echo "$deno_sha256  $deno_archive" | sha256sum -c -; \
    install -d /opt/deno/bin; \
    unzip -q "$deno_archive" deno -d /opt/deno/bin; \
    curl --fail --silent --show-error --location --retry 3 --retry-delay 2 \
        --connect-timeout 15 --max-time 300 \
        "https://releases.astral.sh/github/uv/releases/download/${UV_VERSION}/uv-${deno_arch}-unknown-linux-gnu.tar.gz" \
        -o "$uv_archive"; \
    echo "$uv_sha256  $uv_archive" | sha256sum -c -; \
    install -d /tmp/uv; \
    tar -xzf "$uv_archive" -C /tmp/uv --no-same-owner --strip-components=1; \
    install -Dm755 /tmp/uv/uv /usr/local/bin/uv; \
    install -Dm755 /tmp/uv/uvx /usr/local/bin/uvx; \
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
