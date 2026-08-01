# acp-agent

CLI and Rust library for discovering, installing, running, and serving
[Agent Client Protocol (ACP)](https://agentclientprotocol.com/) agents.

## Install

```sh
curl -fsSL https://github.com/OpenInsightDev/acp-agent/releases/latest/download/install.sh | sh
```

## Quick start

Search the registry and install an agent:

```sh
acp-agent list
acp-agent search codex
acp-agent install-env --yes
acp-agent install codex-acp
```

`install-env` installs Deno or uv when a compatible JavaScript or Python
toolchain is unavailable. Binary distributions are downloaded, validated, and
stored in the platform cache.

Run an installed agent over stdio:

```sh
acp-agent run codex-acp
```

Registry arguments and environment variables are applied first. Additional
arguments are passed to the agent:

```sh
acp-agent run codex-acp --model gpt-5
```

## Serve over HTTP

Expose an agent through ACP HTTP/SSE and WebSocket transports:

```sh
acp-agent serve codex-acp --host 127.0.0.1 --port 8010
```

The server exposes:

| URL | Purpose |
| --- | --- |
| `http://127.0.0.1:8010/acp` | ACP over HTTP/SSE |
| `ws://127.0.0.1:8010/acp` | ACP over WebSocket |
| `http://127.0.0.1:8010/health` | Health check; returns `ok` |

Both ACP transports use `/acp` by default. Each connection starts an independent
agent process. Use `--path` to change the ACP endpoint and `--no-health` to
disable the health check:

```sh
acp-agent serve codex-acp --port 8010 --path /agent --no-health
```

Browser cross-origin access is disabled by default. Origins can be repeated, or
all origins can be explicitly allowed:

```sh
acp-agent serve codex-acp --port 8010 \
  --cors-origin https://app.example.com \
  --cors-origin http://localhost:3000
acp-agent serve codex-acp --port 8010 --allow-any-origin
```

Arguments after `--` are passed to the agent:

```sh
acp-agent serve codex-acp --port 8010 -- --model gpt-5
```

The default port is `0`, which lets the operating system select an available
port. Set an explicit port when another process or container needs to connect.

Authentication belongs to the served agent. A client may be able to connect and
initialize without credentials while authenticated operations, such as creating
a session, still fail. Configure credentials according to the agent's own
documentation.

## Docker

Build the current project, install `codex-acp` inside the container, and expose
it to clients on the host:

```sh
docker build -t acp-agent:local .

docker run --rm \
  -p 127.0.0.1:8010:8010 \
  --entrypoint sh \
  acp-agent:local -lc \
  'acp-agent install-env --yes &&
   acp-agent install codex-acp &&
   exec acp-agent serve codex-acp --host 0.0.0.0 --port 8010'
```

From the host, use `http://127.0.0.1:8010/acp` for HTTP/SSE or
`ws://127.0.0.1:8010/acp` for WebSocket. A `GET` request to
`http://127.0.0.1:8010/health` should return `ok`.

The image does not include agent credentials. Pass only the environment or
credential storage required by the selected agent. Without Codex credentials,
`codex-acp` initializes and reports its authentication methods, but session
creation returns an authentication error.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Rust dependency

The server is implemented with
[`agent-client-protocol-http` 2.0](https://docs.rs/agent-client-protocol-http/2.0.0/agent_client_protocol_http/)
and its `server` feature.

## License

MIT
