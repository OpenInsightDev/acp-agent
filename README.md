# acp-agent

CLI and Rust library for discovering, installing, running, and serving [Agent Client Protocol (ACP)](https://agentclientprotocol.com/) agents.

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
Additional arguments are passed to the agent;
hyphen-prefixed arguments must come after the `--` separator:

```sh
acp-agent run codex-acp -- --model gpt-5
```

Run an agent with its yolo/auto-approve mode enabled:

```sh
acp-agent run gemini --yolo
acp-agent run claude-acp --yolo -- --model opus
```

`--yolo` injects the agent's mapped startup flag, e.g. `--yolo` for Gemini, `--dangerously-skip-permissions` for Claude, `--dangerously-skip-sandbox-and-permissions` for Codex.

> The yolo-mode catalog can be fetched from the CDN (<https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json>).

## Serve over HTTP

Expose an agent through ACP HTTP/SSE and WebSocket transports:

```sh
acp-agent serve codex-acp --host 127.0.0.1 --port 8010
```

<a id="serve-parameters"></a>

`serve` takes the following arguments:

| Argument                 | Default     | Description                                                                  |
| ------------------------ | ----------- | ---------------------------------------------------------------------------- |
| `<agent-id>`             | _(required)_ | Agent to serve.                                                              |
| `--host <host>`          | `127.0.0.1` | Hostname or IP address for the HTTP listener.                                |
| `--port <port>`          | `0`         | TCP port for the HTTP listener. `0` lets the operating system pick a port.   |
| [`--subpath <path>`](#serve-subpath)     | _(none)_    | URL prefix applied to all served endpoints (ACP, health, readyz).            |
| [`--agent-sub-path`](#serve-subpath)     | `false`     | Use the agent id as the subpath (equivalent to `--subpath /<agent-id>`).     |
| [`--path <path>`](#serve-subpath)        | `/acp`      | ACP HTTP/SSE and WebSocket endpoint path.                                    |
| [`--cors-origin <origin>`](#cors)        | _(none)_    | Browser origin allowed to access the endpoint. May be repeated.              |
| [`--allow-any-origin`](#cors)            | `false`     | Allow requests from every browser origin.                                    |
| [`--no-health`](#health-and-readiness)   | `false`     | Disable the `GET /health` endpoint.                                          |
| [`--no-readyz`](#health-and-readiness)   | `false`     | Disable the `GET /readyz` agent readiness endpoint.                          |
| `--max-processes <n>`   | `16`        | Maximum concurrent agent processes for this served route.                    |
| `--yolo`                 | `false`     | Activate the agent's yolo/auto-approve mode (injects the mapped startup flag).|
| [`-- <args>`](#arguments)    | _(none)_    | Arguments passed to the agent process.                                       |

The server exposes:

| URL                            | Purpose                                                    |
| ------------------------------ | ---------------------------------------------------------- |
| `http://127.0.0.1:8010/acp`    | ACP over HTTP/SSE                                          |
| `ws://127.0.0.1:8010/acp`      | ACP over WebSocket                                         |
| `http://127.0.0.1:8010/health` | Liveness check; returns `ok`                               |
| `http://127.0.0.1:8010/readyz` | Agent readiness; `503` with the last launch failure detail |

Both ACP transports use `/acp` by default.
Each connection starts an independent agent process.
When the process limit is exhausted, new initial connections receive HTTP `503` while health and readiness probes remain available.
Use `--path` to change the ACP endpoint, `--no-health` to disable the health check, and `--no-readyz` to disable the readiness probe.

Use [`--subpath`](#serve-subpath) to serve under a URL prefix, e.g. a reverse-proxy mount point or a shared host path:

<a id="serve-subpath"></a>

```sh
acp-agent serve codex-acp --port 8010 --subpath /myapp --path /rpc
# ACP at http://127.0.0.1:8010/myapp/rpc, health at .../myapp/health
```

### Health and readiness

`/health` only reflects the HTTP server.
`/readyz` reflects agent-process health: it returns `200 ready` while the most recent agent launch succeeded, and `503` plus the last failure (including the agent's stderr tail) after a launch failure.
Agent stderr is also forwarded to the serve process's logs, so startup failures such as a missing agent executable or a failed package install are visible in `docker logs` instead of being swallowed by the connection error response.

### CORS

Browser cross-origin access is disabled by default.
Origins can be repeated, or all origins can be explicitly allowed:

```sh
acp-agent serve codex-acp --port 8010 \
  --cors-origin https://app.example.com \
  --cors-origin http://localhost:3000
acp-agent serve codex-acp --port 8010 --allow-any-origin
```

### Arguments

Arguments after `--` are passed to the agent:

```sh
acp-agent serve codex-acp --port 8010 -- --model gpt-5
```

## Named servers

Start a background server and register one or more agents below it:

```sh
acp-agent server start --host 127.0.0.1 --port 8010
acp-agent server register codex-acp
acp-agent server register claude --route /reviewer -- --model opus
```

`server` exposes the following subcommands and arguments:

| Subcommand  | Argument                      | Default     | Description                                                              |
| ----------- | ----------------------------- | ----------- | ------------------------------------------------------------------------ |
| `start`     | `--name <name>`               | `default`   | Local server name used by later commands.                                |
|             | `--host <host>`               | `127.0.0.1` | Hostname or IP address for the named server listener.                    |
|             | `--port <port>`               | `8010`      | TCP port for the named server listener. Use `0` for an ephemeral port.   |
| `stop`      | `--name <name>`               | `default`   | Local server name.                                                       |
| `register`  | `<agent-id>`                  | _(required)_ | Agent to register under this server.                                    |
|             | `--name <name>`               | `default`   | Target server name.                                                      |
|             | `--route <path>` (`--subpath`) | `/<agent-id>` | Public route prefix.                                                    |
|             | serve-like settings            | —           | [`--path`](#serve-parameters), repeated [`--cors-origin`](#serve-parameters), [`--allow-any-origin`](#serve-parameters), [`--no-health`](#serve-parameters), [`--no-readyz`](#serve-parameters), [`--max-processes`](#serve-parameters), [`--yolo`](#serve-parameters), and trailing [`-- <args>`](#serve-parameters), as in the [serve parameter table](#serve-parameters). |
| `unregister`| `<agent-id>`                  | _(required)_ | Agent to remove from the server.                                        |
|             | `--name <name>`               | `default`   | Target server name.                                                      |
| `list`      | —                             | —           | List every recorded named server and its lifecycle state.                |
|             | `--json`                      | off         | Emit server records as structured JSON (deterministic field order).      |
| `status`    | `--name <name>`               | `default`   | Show one server's state, configured and actual address, PID, and version.|
|             | `--json`                      | off         | Emit the server record as structured JSON.                               |
| `registrations`| `--name <name>`             | `default`   | List registered agent IDs, route prefixes, and readiness.                |
|             | `--json`                      | off         | Emit registration records as structured JSON.                            |
| `logs`      | `--name <name>`               | `default`   | Tail a named server's log.                                                |
|             | `--lines <n>`                 | `50`        | Number of log lines to tail.                                             |
|             | `--json`                      | off         | Emit `{name, lines}` with the tail.                                      |

The server name defaults to `default`.
Use `--name` on every command to manage another server independently:

```sh
acp-agent server start --name work --port 8020
acp-agent server register codex-acp --name work
acp-agent server unregister codex-acp --name work
acp-agent server stop --name work
```

### Inspection

`server list` reports every recorded server. Each server has one of four states:

- `running` — the daemon answered its control endpoint.
- `starting` — the state file exists and the recorded process is alive, but it has not answered yet.
- `stale` — the state file exists but the recorded process is gone (crash or unclean shutdown); starting the server again replaces it.
- `stopped` — no state file exists (`status` only).

```sh
acp-agent server list
acp-agent server status --name work
acp-agent server registrations --name work
acp-agent server logs --name work --lines 20
```

`server registrations` probes each route's `/readyz` endpoint and reports `ready`, `not_ready` (with the failure detail), `disabled` (no readyz endpoint), or `unknown`.
`server logs` tails the daemon's log file.
Add `--json` to any of these commands for automation-friendly output with deterministic field ordering:

```sh
acp-agent server list --json | jq '.[] | select(.state == "stale") | .name'
acp-agent server registrations --name work --json | jq '.[] | select(.readiness != "ready")'
acp-agent server logs --name work --json | jq '.lines'
```

### Routes

By default, `server register <agent-id>` creates the public route `/<agent-id>`.
Its ACP endpoint is `/<agent-id>/acp`, and its health endpoints are `/<agent-id>/health` and `/<agent-id>/readyz`.
`--route` (also accepted as `--subpath`) changes the public route prefix.
The register command accepts the same serve-like endpoint and agent settings as [`serve`](#serve-parameters): `--path`, repeated `--cors-origin`, `--allow-any-origin`, `--no-health`, `--no-readyz`, `--max-processes`, `--yolo`, and trailing agent arguments.

### Management API (advanced)

> This API is primarily intended for the CLI's internal use.

The named server exposes `POST /api/agents` to add a route and `DELETE /api/agents` to remove one.
POST accepts the agent ID, public route, and serve-like settings; it does not accept a target URL or PID:

```json
{
  "id": "demo",
  "route": "/demo",
  "serve": {
    "path": "/acp",
    "cors_origins": [],
    "allow_any_origin": false,
    "health_endpoint": true,
    "readyz_endpoint": true,
    "max_processes": 16,
    "yolo": false,
    "args": ["--model", "gpt-5"]
  }
}
```

DELETE accepts `{"id":"demo"}`.
Registered routes support ACP HTTP/SSE and WebSocket traffic.
Unregistering removes the route for new connections; existing connections are allowed to end naturally.

### State and logs

Named server state and logs live below the platform cache directory.
On Unix, the server directory is mode `0700` and its state and log files are mode `0600`;
Windows uses the current user's cache-directory ACL.

## Local Cache

Binary agents are cached and managed by `acp-agent`. `npx` and `uvx` agents are executed on demand by their respective runners (`npm exec`, `deno x`, and `uvx`); `install` prepares the same runner cache that `run`/`serve` will read, so installation and execution share one lifecycle:

- **npm** — the package is installed globally, which `npm exec` finds first;
- **uv** — the tool is installed with `uv tool install`, which `uvx` prefers;
- **Deno** (when npm is unavailable) — the package is fetched into Deno's npm cache with `deno cache`, which `deno x` reads.

Binary agents are stored in the platform cache directory (`$HOME/.cache/acp-agent` on macOS and Linux, `%LOCALAPPDATA%\acp-agent` on Windows, `/cache/acp-agent` inside the Docker image).

List agents installed locally:

```sh
acp-agent list --installed
```

- Add `--json` to return the installed records as structured JSON, including their cache and executable paths.

Remove an agent from the local cache, and uninstall any npm/uv launcher that `install` created. Deno-managed packages have nothing to remove: their cache belongs to Deno, which garbage-collects it:

```sh
acp-agent uninstall codex-acp
acp-agent uninstall codex-acp claude dev # uninstall multiple agents
```

Digest-keyed binary updates keep older validated cache entries in place so a running server can continue using the executable it already resolved.
`uninstall` removes all cached versions for the agent.

```sh
acp-agent update codex-acp
```

## Docker

The image contains the `acp-agent` CLI and its supported JavaScript/Python toolchains (`deno` and `uv`).
No agent is preloaded into the image; the first `run` or `serve` command downloads or prepares the selected agent as needed.
The final image is a small non-root runtime image, using `acp-agent` as the entrypoint.

```sh
docker build -t acp-agent:latest .

docker run --rm \
  -p 127.0.0.1:8010:8010 \
  -v acp-agent-cache:/cache \
  acp-agent:latest serve codex-acp --host 0.0.0.0 --port 8010
```

Mount the cache dir `/cache` to a named volume or a fixed host temp dir so the same agent's downloaded runtime is reused across containers and cold starts are much faster:

```sh
# named volume
docker run --rm -v acp-agent-cache:/cache acp-agent:latest run codex-acp
# fixed host dir (e.g. under a scratch dir)
docker run --rm -v $HOME/.cache/acp-agent:/cache acp-agent:latest run codex-acp
```

The same form works for every CLI command.

```sh
docker run --rm acp-agent:latest list
docker run --rm acp-agent:latest search codex
docker run --rm -v acp-agent-cache:/cache acp-agent:latest install codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest list --installed
docker run --rm -v acp-agent-cache:/cache acp-agent:latest update codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest uninstall codex-acp
docker run --rm -v acp-agent-cache:/cache acp-agent:latest run codex-acp
```

Binary agent installs append a human-readable line to `/cache/acp-agent/agent-install.log` (successes and failures, with timestamps and the full error chain).

From the host, use `http://127.0.0.1:8010/acp` for HTTP/SSE or `ws://127.0.0.1:8010/acp` for WebSocket.
A `GET` request to `http://127.0.0.1:8010/health` should return `ok`.

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
