#![doc = r#"
`acp-agent` is a CLI for discovering, installing, launching, and proxying ACP agents
from the public registry.

## Command-Line Usage

List every published agent:

```bash
acp-agent list
```

Search for agents by ID, name, or description:

```bash
acp-agent search claude
```

Install an agent from the registry:

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

Expose an agent over a network transport:

```bash
acp-agent serve example-agent --transport http --host 127.0.0.1 --port 8010
acp-agent serve example-agent --transport tcp --port 9000
acp-agent serve example-agent --transport ws --port 7000
```

## Transport Modes

- `http` exposes one HTTP/2 byte stream over `POST /` with `Content-Type: application/octet-stream`.
- `tcp` exposes raw stdin/stdout bytes over a single TCP connection.
- `ws` exposes ACP messages as WebSocket text frames (one message per frame).

## Library Surface

This package primarily exists as a CLI, but the implementation is also exposed as a
library for embedding:

- [`commands`] embeds the CLI parser and dispatch logic.
- [`registry`] loads and queries the public ACP registry.
- [`runtime`] prepares agent commands and serves them over stdio or network transports.
"#]
#![warn(missing_docs)]

/// CLI parsing and command-dispatch helpers used by the `acp-agent` executable.
pub mod commands;
/// Types and helpers for loading ACP agent metadata from the public registry.
pub mod registry;
/// Runtime primitives for preparing agent commands and exposing them over transports.
pub mod runtime;
