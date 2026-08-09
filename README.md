# acp-agent

CLI for discovering, installing, running, and serving [Agent Client Protocol (ACP)](https://agentclientprotocol.com/) agents.

## Install

```sh
curl -fsSL https://github.com/OpenInsightDev/acp-agent/releases/latest/download/install.sh | sh
```

or with `cargo`:

```sh
cargo install acp-agent
```

## Quick start

Search the acp registry and install an agent:

```sh
acp-agent list
acp-agent search codex
acp-agent install-env --yes
acp-agent install codex-acp
# install several agents concurrently
acp-agent install codex-acp claude dev
```

`install-env` installs Deno or uv when a compatible JavaScript or Python toolchain is unavailable.
Binary distributions are downloaded, validated, and stored in the platform cache.

Run an installed agent over stdio:

```sh
acp-agent run codex-acp
```

Registry arguments and environment variables are applied first.
Additional arguments are passed to the agent; hyphen-prefixed arguments must come after the `--` separator:

```sh
acp-agent run codex-acp -- --model gpt-5
```

Run an agent with its yolo/auto-approve mode enabled:

```sh
acp-agent run gemini --yolo
acp-agent run claude-acp --yolo -- --model opus
```

`--yolo` injects the agent's mapped startup flag, e.g. `--yolo` for Gemini, `--dangerously-skip-permissions` for Claude, `--dangerously-skip-sandbox-and-permissions` for Codex.

> The yolo-mode catalog is fetched from the CDN (<https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json>) to get the latest version.
> If the network is unavailable, it falls back to the offline copy bundled with this release, so `--yolo` keeps working offline.

## Serve over HTTP

Expose an agent through ACP HTTP/SSE and WebSocket transports:

```sh
acp-agent serve codex-acp --host 127.0.0.1 --port 8010
```

The server exposes:

| URL                            | Purpose                                                    |
| ------------------------------ | ---------------------------------------------------------- |
| `http://127.0.0.1:8010/acp`    | ACP over HTTP/SSE                                          |
| `ws://127.0.0.1:8010/acp`      | ACP over WebSocket                                         |
| `http://127.0.0.1:8010/health` | Liveness check; returns `ok`                               |
| `http://127.0.0.1:8010/readyz` | Agent readiness; `503` with the last launch failure detail |

Both ACP transports use `/acp` by default.
Each connection starts an independent agent process.
Use `--path` to change the ACP endpoint, `--no-health` to disable the health check, and `--no-readyz` to disable the readiness probe:

```sh
acp-agent serve codex-acp --port 8010 --path /agent --no-health
```

Use `--subpath` to serve the whole tree (ACP endpoint, `/health`, and `/readyz`) under a URL prefix, e.g. a reverse-proxy mount point or a shared host path:

```sh
acp-agent serve codex-acp --port 8010 --subpath /myapp --path /rpc
# ACP at http://127.0.0.1:8010/myapp/rpc, health at .../myapp/health
```

`/health` only reflects the HTTP server.
`/readyz` reflects agent-process health: it returns `200 ready` while the most recent agent launch succeeded, and `503` plus the last failure (including the agent's stderr tail) after a launch failure.
Agent stderr is also forwarded to the serve process's logs, so startup failures such as a missing agent executable or a failed package install are visible in `docker logs` instead of being swallowed by the connection error response.

Browser cross-origin access is disabled by default.
Origins can be repeated, or all origins can be explicitly allowed:

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

The default port is `0`, which lets the operating system select an available port.
Set an explicit port when another process or container needs to connect.

## Manage the local cache

Binary agents are stored in the platform cache directory (`$HOME/.cache/acp-agent` on macOS and Linux, `%LOCALAPPDATA%\acp-agent` on Windows, `/cache/acp-agent` inside the Docker image).

List agents installed locally (`id`, `version`, `platform`, and cache directory):

```sh
acp-agent list --installed
```

Add `--json` to return the installed records as structured JSON, including
their cache and executable paths.

Remove an agent from the local cache, and uninstall its globally installed npm/uv wrapper when it ships as a package:

```sh
acp-agent uninstall codex-acp
```

Multiple agents (install, update, and uninstall) are handled concurrently, e.g.:

```sh
acp-agent update codex-acp claude dev
```

Refresh an agent to the registry's latest release.
Stale cached versions are discarded before the preferred distribution is (re)installed:

```sh
acp-agent update codex-acp
```

Package-manager installs (`npm`, `deno`, `uv`) live in the global toolchain rather than the agent cache, so `list --installed` only reports cached binary distributions.

## Docker

The image contains the `acp-agent` CLI and its supported JavaScript/Python toolchains (`deno` and `uv`).
It does not install a specific agent during the image build.
The final image is a small non-root runtime image, and the CLI is its entrypoint, so arguments can be passed directly after the image name.

```sh
docker build -t acp-agent:latest .

docker run --rm \
  -p 127.0.0.1:8010:8010 \
  -v acp-agent-cache:/cache \
  acp-agent:latest serve codex-acp --host 0.0.0.0 --port 8010
```

Mount the cache dir to a named volume or a fixed host temp dir so the same agent's downloaded runtime is reused across containers and cold starts are much faster:

```sh
# named volume
docker run --rm -v acp-agent-cache:/cache acp-agent:latest run codex-acp
# fixed host dir (e.g. under a scratch dir)
docker run --rm -v $HOME/.cache/acp-agent:/cache acp-agent:latest run codex-acp
```

The same form works for every CLI command.
`install-env` is optional in this image because Deno and uv are installed at image build time.
The `acp-agent` binary lives at `/acp-agent` in the image, so you can vendor it into your own image with a `COPY --from=` stage (the same pattern uv offers for its binary), e.g.:

```dockerfile
COPY --from=ghcr.io/openinsightdev/acp-agent:latest /usr/local/bin/acp-agent /usr/local/bin/acp-agent
```

```sh
docker run --rm acp-agent:latest list
docker run --rm acp-agent:latest search codex
docker run --rm acp-agent:latest install-env --yes
docker run --rm -v acp-agent-cache:/cache acp-agent:latest install codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest list --installed
docker run --rm -v acp-agent-cache:/cache acp-agent:latest update codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest uninstall codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest run codex-acp
```

`/cache` stores the registry and downloaded agent cache.
No agent is preloaded into the image; the first `run` or `serve` command downloads or prepares the selected agent as needed.

Binary agent installs append a human-readable line to `/cache/acp-agent/agent-install.log` (successes and failures, with timestamps and the full error chain).
The image has no shell, so when a container fails to prepare an agent, retrieve that log from the host with:

```sh
docker cp <container>:/cache/acp-agent/agent-install.log .
```

Arguments after the image name are passed to `acp-agent`, including agent arguments after the relevant `--` separator:

```sh
docker run --rm -v acp-agent-cache:/cache acp-agent:latest \
  serve codex-acp --host 0.0.0.0 --port 8010 -- --model gpt-5
```

From the host, use `http://127.0.0.1:8010/acp` for HTTP/SSE or `ws://127.0.0.1:8010/acp` for WebSocket.
A `GET` request to `http://127.0.0.1:8010/health` should return `ok`.

The image does not include agent credentials.
Pass only the environment or credential storage required by the selected agent.

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
```

## Rust dependency

The server is implemented with [`agent-client-protocol-http` 2.0](https://docs.rs/agent-client-protocol-http/2.0.0/agent_client_protocol_http/) and its `server` feature.

## Contribution

If you know how to enable yolo mode for the acp agent you are using, you are welcome to add new entries to the `data/yolo-modes.json` list.

## License

MIT
