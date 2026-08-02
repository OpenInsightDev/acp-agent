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
arguments are passed to the agent; hyphen-prefixed arguments must come after
the `--` separator:

```sh
acp-agent run codex-acp -- --model gpt-5
```

Run an agent with its yolo/auto-approve mode enabled:

```sh
acp-agent run gemini --yolo
acp-agent run claude-acp --yolo -- --model opus
```

`--yolo` injects the agent's mapped startup flag (fetched from the published
yolo-mode catalog at `https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json`),
e.g. `--yolo` for Gemini, `--dangerously-skip-permissions` for Claude,
`--dangerously-skip-sandbox-and-permissions` for Codex. Agents that only expose
yolo over the ACP wire protocol (e.g. `session/set_mode`) fail loudly with
guidance instead of silently skipping the requested behavior.

## Serve over HTTP

Expose an agent through ACP HTTP/SSE and WebSocket transports:

```sh
acp-agent serve codex-acp --host 127.0.0.1 --port 8010
```

The server exposes:

| URL                            | Purpose                    |
| ------------------------------ | -------------------------- |
| `http://127.0.0.1:8010/acp`    | ACP over HTTP/SSE          |
| `ws://127.0.0.1:8010/acp`      | ACP over WebSocket         |
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

### Argument boundary

`acp-agent`'s own options (`--host`/`--port`/`--path` on `serve`) must come
**before** the `--` separator. Hyphen-prefixed agent arguments must come after
`--` for both `run` and `serve`; anything after `--` is passed through to the
agent untouched. Omit `--` and a hyphen-prefixed agent argument (such as
`--model`) is rejected with a clap error that hints at `--`, instead of being
silently forwarded.

Authentication belongs to the served agent. A client may be able to connect and
initialize without credentials while authenticated operations, such as creating
a session, still fail. Configure credentials according to the agent's own
documentation.

## Docker

The image contains the `acp-agent` CLI and its supported JavaScript/Python
toolchains (`deno`, `uv`, and `uvx`). It does not install a specific agent during
the image build. The final image is a small non-root runtime image, and the CLI
is its entrypoint, so arguments can be passed directly after the image name.

```sh
docker build -t acp-agent:local .

docker run --rm \
  -p 127.0.0.1:8010:8010 \
  -v acp-agent-cache:/cache \
  acp-agent:local serve codex-acp --host 0.0.0.0 --port 8010
```

The same form works for every CLI command. `install-env` is optional in this
image because Deno and uv are installed at image build time:

```sh
docker run --rm acp-agent:local list
docker run --rm acp-agent:local search codex
docker run --rm acp-agent:local install-env --yes
docker run --rm -v acp-agent-cache:/cache acp-agent:local install codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:local run codex-acp
```

`/cache` stores the registry and downloaded agent cache. Mount a named volume
when containers are recreated. No agent is preloaded into the image; the first
`install`, `run`, or `serve` command downloads or prepares the selected agent as
needed.

Arguments after the image name are passed to `acp-agent`, including agent
arguments after the relevant `--` separator:

```sh
docker run --rm -v acp-agent-cache:/cache acp-agent:local \
  serve codex-acp --host 0.0.0.0 --port 8010 -- --model gpt-5
```

From the host, use `http://127.0.0.1:8010/acp` for HTTP/SSE or
`ws://127.0.0.1:8010/acp` for WebSocket. A `GET` request to
`http://127.0.0.1:8010/health` should return `ok`.

The image does not include agent credentials. Pass only the environment or
credential storage required by the selected agent. Without Codex credentials,
`codex-acp` initializes and reports its authentication methods, but session
creation returns an authentication error.

The image runs as UID `65532` and does not provide a shell. Use the CLI
entrypoint directly; do not rely on `--entrypoint sh` for container setup.

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
