# Issue: ACP connections have unbounded message queues

## Status

Ignored (won't fix in this repository); root cause is upstream.

## Why

`agent-client-protocol-http 2.0.0` uses `tokio::sync::mpsc::unbounded_channel` for per-connection inbound and outbound traffic (`src/connection.rs:466-467`).
Bounding only those channels would not bound memory: the inbound pump drains them instantly into `agent-client-protocol`'s `Channel`, which is also `mpsc::unbounded`, and that is the queue that actually grows while a slow agent consumes stdin.
A real fix requires converting `Channel` to bounded mpsc in `agent-client-protocol` and updating every `unbounded_send` call site in both crates (~26k lines, ~30 sites, several in sync code).
The recommended service-boundary guard (connection and request limits) is not enforceable from this repository because the library creates connections internally and does not expose the connection count.

## Follow-up

Track upstream (`github.com/agentclientprotocol/rust-sdk`) for bounded transport channels.
