# acp-agent

CLI and Rust library for discovering, installing, running, and serving [Agent Client Protocol (ACP)](https://agentclientprotocol.com/) agents.

## Install

```sh
curl -fsSL https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/scripts/install.sh | sh
```

or with `cargo`:

```sh
cargo install acp-agent
```

The installer fetches the binary from the [jsDelivr CDN](https://www.jsdelivr.com/):
binaries are published to npm as platform packages
(`@open-insight/acp-agent-<platform>`, built with [cargo-npm](https://github.com/abemedia/cargo-npm))
and served from `https://cdn.jsdelivr.net/npm/@open-insight/acp-agent-<platform>@<version>/acp-agent`.
The installer always resolves the exact version to install (from `ACP_AGENT_VERSION`, or by
following the GitHub `latest` redirect) and verifies the CDN binary reports that version;
if the CDN is unreachable or mismatched, it falls back to the GitHub release archives
(verified against SHA256SUMS).

## Quick start

Search the acp registry and install an agent:

```sh
acp-agent list
acp-agent search codex
acp-agent install-env --yes
acp-agent install codex-acp
# install several agents concurrently
acp-agent install codex-acp claude-acp devin
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

`--yolo` injects the agent's mapped startup flag, e.g. `--yolo` for Gemini, `--dangerously-skip-permissions` for Claude, `--dangerously-skip-sandbox-and-permissions` for Codex. The catalog supports startup CLI flags only; ACP session modes and config options are not handled by this command.

> The yolo-mode catalog can be fetched from the CDN (<https://cdn.jsdelivr.net/gh/OpenInsightDev/acp-agent@main/data/yolo-modes.json>). Each entry must contain a `flag` string; entries for unsupported protocol-level modes are invalid.

## Serve over HTTP

Expose an agent through ACP HTTP/SSE and WebSocket transports:

```sh
acp-agent serve codex-acp --host 127.0.0.1 --port 8010
```

<a id="serve-parameters"></a>

`serve` takes the following arguments:

| Argument                               | Default      | Description                                                                    |
| -------------------------------------- | ------------ | ------------------------------------------------------------------------------ |
| `<agent-id>`                           | _(required)_ | Agent to serve.                                                                |
| `--host <host>`                        | `127.0.0.1`  | Hostname or IP address for the HTTP listener.                                  |
| `--port <port>`                        | `0`          | TCP port for the HTTP listener. `0` lets the operating system pick a port.     |
| [`--subpath <path>`](#serve-subpath)   | _(none)_     | URL prefix applied to all served endpoints (ACP, health, readyz).              |
| [`--agent-sub-path`](#serve-subpath)   | `false`      | Use the agent id as the subpath (equivalent to `--subpath /<agent-id>`).       |
| [`--path <path>`](#serve-subpath)      | `/acp`       | ACP HTTP/SSE and WebSocket endpoint path.                                      |
| [`--cors-origin <origin>`](#cors)      | _(none)_     | Browser origin allowed to access the endpoint. May be repeated.                |
| [`--allow-any-origin`](#cors)          | `false`      | Allow requests from every browser origin.                                      |
| [`--no-health`](#health-and-readiness) | `false`      | Disable the `GET /health` endpoint.                                            |
| [`--no-readyz`](#health-and-readiness) | `false`      | Disable the `GET /readyz` agent readiness endpoint.                            |
| `--max-processes <n>`                  | `16`         | Maximum concurrent agent processes for this served route.                      |
| `--yolo`                               | `false`      | Activate the agent's yolo/auto-approve mode (injects the mapped startup flag). |
| [`-- <args>`](#arguments)              | _(none)_     | Arguments passed to the agent process.                                         |

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

Named servers are live in-memory instances owned by one foreground daemon. Start that daemon in one terminal:

```sh
acp-agent daemon
```

The daemon accepts CLI control requests on one user-scoped Unix socket. Server commands start the daemon automatically when no daemon is reachable, so running it explicitly is also useful under a service manager. Set `ACP_AGENT_DAEMON_SOCKET` to override the socket path.

In another terminal, create an instance and register agents below it:

```sh
acp-agent server start --host 127.0.0.1 --port 8010
acp-agent server register codex-acp
acp-agent server register claude --route /reviewer -- --model opus
```

Each named instance owns its public TCP listener and route table inside the daemon. The management socket is separate from those public listeners; named instances expose registered ACP routes, not management operations. The default server name is `default`; use `--name` to manage another instance:

```sh
acp-agent server start --name work --port 8020
acp-agent server register codex-acp --name work
acp-agent server unregister codex-acp --name work
acp-agent server stop --name work
```

`server` exposes these subcommands:

| Subcommand      | Arguments                                                     | Description                                                                                   |
| --------------- | ------------------------------------------------------------- | --------------------------------------------------------------------------------------------- |
| `start`         | `--name <name>`, `--host <host>`, `--port <port>`             | Create or reuse a named instance and its public listener. Use port `0` for an ephemeral port. |
| `stop`          | `--name <name>`                                               | Stop and remove a named instance from the daemon.                                             |
| `register`      | `<agent-id>`, `--name <name>`, `--route <path>` (`--subpath`) | Register an agent route with a named instance.                                                |
| `unregister`    | `<agent-id>`, `--name <name>`                                 | Remove an agent route from a named instance.                                                  |
| `list`          | `--json`                                                      | List live named instances owned by the daemon.                                                |
| `status`        | `--name <name>`, `--json`                                     | Show one live instance's state, listener address, and configuration.                          |
| `registrations` | `--name <name>`, `--json`                                     | List routes and readiness reported by the daemon.                                             |

The `register` command accepts the same endpoint and agent settings as [`serve`](#serve-parameters): `--path`, repeated [`--cors-origin`](#serve-parameters), [`--allow-any-origin`](#serve-parameters), [`--no-health`](#serve-parameters), [`--no-readyz`](#serve-parameters), [`--max-processes`](#serve-parameters), `--yolo`, and trailing agent arguments.

### Inspection

`server list`, `server status`, and `server registrations` read the daemon's current in-memory state. `server registrations` uses readiness snapshots maintained by each route runtime; it does not probe public routes from the CLI. Readiness is reported as `ready`, `not_ready` with a failure detail, or `disabled` when the route has no readiness endpoint.

```sh
acp-agent server list
acp-agent server status --name work
acp-agent server registrations --name work
```

Add `--json` to these commands for automation-friendly structured output:

```sh
acp-agent server list --json | jq '.[] | .name'
acp-agent server registrations --name work --json | jq '.[] | select(.readiness != "ready")'
```

Named instances and registrations exist only in the running daemon's memory. They are not restored after the daemon restarts; create and register them again.

### Routes

By default, `server register <agent-id>` creates the public route `/<agent-id>`. Its ACP endpoint is `/<agent-id>/acp`, and its health endpoints are `/<agent-id>/health` and `/<agent-id>/readyz`. `--route` (also accepted as `--subpath`) changes the public route prefix.

Registered routes support ACP HTTP/SSE and WebSocket traffic. Unregistering removes the route for new connections; existing connections are allowed to end naturally.

### Logging

The daemon and registered agents write diagnostics to standard error. Run `acp-agent daemon` under a service manager to collect and retain those logs, or redirect the daemon's standard error when running it directly.

## Local Cache

Binary agents are cached and managed by `acp-agent`. Package-based agents are installed and run through the same runner resolution used by `run` and `serve`: npm distributions use `npm install --global` and `npm exec` when npm is available, falling back to Deno's npm cache and `deno x` when npm is unavailable; uvx distributions use `uv tool install` and `uvx`.

Binary agents are stored below the platform cache directory returned by the operating system (`~/Library/Caches/acp-agent` on macOS by default, `$XDG_CACHE_HOME/acp-agent` or `$HOME/.cache/acp-agent` on Linux, `%LOCALAPPDATA%\acp-agent` on Windows, and `/cache/acp-agent` inside the Docker image).

List cached binary agents locally:

```sh
acp-agent list --installed
```

- Add `--json` to return the cached binary records as structured JSON, including their cache and executable paths. Package-manager installations are not included.

Remove a cached binary, or uninstall a package-based agent through the package manager that installed it. For Deno-installed npm distributions, there is no global launcher to remove because Deno owns the npm cache:

```sh
acp-agent uninstall codex-acp
acp-agent uninstall codex-acp claude-acp devin # uninstall multiple agents
```

For binary distributions, `update` installs and verifies the replacement first, then removes older cached versions for the same agent and platform. Digest-keyed cache entries remain available until cleanup, while a running server can continue using the executable it already resolved. Package-based distributions are updated through their package manager:

```sh
acp-agent update codex-acp
```

## Docker

The image contains the `acp-agent` CLI and its supported JavaScript/Python toolchains (`deno` and `uv`).
No agent is preloaded into the image; the first `run` or `serve` command downloads or prepares the selected agent as needed.
The final image is a small Debian runtime image that uses `acp-agent` as the entrypoint and runs as root by default.

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
docker run --rm -v "$PWD/acp-agent-cache:/cache" acp-agent:latest run codex-acp
```

The same form works for one-shot CLI commands. Named servers are background daemons and should be managed from a host or a long-lived container; a `docker run --rm ... server start` container exits when the CLI returns and cannot keep the daemon alive.

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

If you know a startup CLI flag that enables yolo mode for the ACP agent you are using, you are welcome to add an entry to `data/yolo-modes.json`. The catalog accepts only entries such as `{ "flag": "--yolo" }`; protocol-level modes and config options do not belong in this file.

Releases are published to npm automatically (see `release.yml`). Publishing uses npm
[trusted publishing](https://docs.npmjs.com/trusted-publishers/) (OIDC), with `NPM_TOKEN`
only as the first-publish fallback. Once the five `@open-insight/acp-agent*` packages are on
npm, configure each one on npmjs.com (Settings → Trusted publishing → GitHub Actions:
`OpenInsightDev`/`acp-agent`, workflow `release.yml`, allow `npm publish`), then remove the
`NPM_TOKEN` secret and the `NODE_AUTH_TOKEN` env in `release.yml`.

## License

MIT
