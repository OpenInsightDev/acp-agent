FROM rust:1.96-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --locked

FROM debian:bookworm-slim

ENV HOME=/root
ENV PATH=/root/.deno/bin:/root/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
    bash \
    ca-certificates \
    curl \
    unzip \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/acp-agent /usr/local/bin/acp-agent

WORKDIR /workspace

ENTRYPOINT ["acp-agent"]
CMD ["--help"]
