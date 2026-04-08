# acp-agent

CLI for discovering, installing, launching, and proxying ACP agents from the public registry.

## Install

```sh
curl -fsSL https://github.com/OpenInsightDev/acp-agent/releases/latest/download/install.sh | sh
```

## Usage

List every published agent:

```bash
acp-agent list
```

Search for agents by ID, name, or description:

```bash
acp-agent search claude
```

Prepare an agent for local execution:

```bash
acp-agent install example-agent
```

Install local runtime prerequisites such as Bun or uv when they are missing:

```bash
acp-agent install-env
```

Run an agent over stdio and pass through extra arguments after `--`:

```bash
acp-agent run example-agent -- --model gpt-5
```

For binary agents, `install` prewarms the local `acp-agent` cache and `run` /
`serve` reuse that cache automatically.

Expose an agent over a network transport:

```bash
acp-agent serve example-agent --transport http --host 127.0.0.1 --port 8010
acp-agent serve example-agent --transport tcp --port 9000
acp-agent serve example-agent --transport ws --port 7000
acp-agent serve example-agent --transport uds --unix-socket /tmp/acp-agent.sock
```

### Transport Modes

- `http` exposes one HTTP/2 byte stream over `POST /` with `Content-Type: application/octet-stream`.
- `tcp` exposes raw stdin/stdout bytes over a single TCP connection.
- `ws` exposes ACP messages as WebSocket text frames (one message per frame).
- `uds` exposes raw stdin/stdout bytes over a single Unix domain socket connection on Unix.


## License

MIT
