# Serve

`acp-agent serve <AGENT_ID>` exposes one catalog agent over HTTP on a listener the process owns, and it carries ACP over HTTP/SSE and WebSocket.
It commits to these outcomes, in this order:

- It MUST announce the address it bound on standard error, so a caller can reach a `--port 0` listener. (`startup::address_lines`)
- On one path it MUST carry ACP over HTTP/SSE and over WebSocket. (`endpoint::http_initialize`, `endpoint::sse_responses`, `endpoint::websocket`)
- `GET /health` MUST answer `200` with the body `ok`, and `--no-health` MUST remove it. (`health::ok`, `health::disabled`)
- `GET /readyz` MUST report agent launch health, answering `ready` before any failed launch and `503` with the last failure after one, and `--no-readyz` MUST remove it. (`readiness::ready`, `readiness::failure_detail`, `readiness::disabled`)
- A browser origin MUST be refused unless it is allowed, and allowing every origin MUST NOT be combined with naming origins. (`cors::disabled_by_default`, `cors::allowed_origins`, `cors::allow_any_origin`, `cors::rejects_conflicting_options`)
- Every endpoint MUST move under a mount prefix when one is requested. (`paths::mount_prefix`)
- Concurrent agent processes MUST be bounded, and overload MUST NOT take the probes down. (`limit::rejects_overload`)
- `SIGTERM` or `SIGINT` MUST stop the process, closing what is still open, and MUST exit `0`. (`shutdown::termination_signal`, `shutdown::closes_active_stream`)

Named instances that register the same transports behind `server` routes are [`Server.md`](Server.md)'s subject.

## Running the server

`acp-agent serve <AGENT_ID> [OPTIONS] [-- <ARGS>...]` starts one long-lived process.
Arguments after `--` are passed to the agent process, and `--yolo` maps the agent's auto-approve arguments, exactly as `run` does.

The listener options are:

| Option | Default | Meaning |
| --- | --- | --- |
| `--host <HOST>` | `127.0.0.1` | Address to bind. |
| `--port <PORT>` | `0` | TCP port; `0` binds an ephemeral port. |

The process MUST print its bound address on standard error and MUST leave standard output empty. (`startup::address_lines`)
It always prints the ACP address, and prints the readiness address only while that probe is enabled. (`startup::address_lines`, `readiness::disabled`)

```text
$ acp-agent serve mock-binary --port 50111
Serving ACP agent at http://127.0.0.1:50111/acp (WebSocket available on the same endpoint)
Agent readiness probe at http://127.0.0.1:50111/readyz
```

The examples here quote one run, so the port is that run's; with the default `--port 0` the printed port is ephemeral.
`--port` MUST be the port the process binds, so the printed address is where it answers. (`startup::requested_port`)

From that address the process serves three endpoints:

| Endpoint | Path | Methods |
| --- | --- | --- |
| ACP | `--path`, default `/acp` | `POST`, `GET`, `DELETE`, and a WebSocket upgrade |
| Health | `/health`, unless `--no-health` | `GET` |
| Readiness | `/readyz`, unless `--no-readyz` | `GET` |

Each endpoint is served at `<mount><path>`, where `<mount>` is empty unless a mount prefix is requested. (`paths::mount_prefix`)
A request for any other path MUST answer `404`. (`paths::custom_endpoint_path`)

A command that cannot start MUST fail with exit status `1`, name the failure on standard error, and leave standard output empty. (`startup::rejects_unknown_agent`)
A combination of options that cannot apply together is rejected before startup with exit status `2` and a message on standard error. (`cors::rejects_conflicting_options`, `paths::rejects_conflicting_mounts`)

### Tests

- `startup::address_lines`: the process prints the ACP line and the readiness line on standard error, prints nothing on standard output, binds loopback by default, and answers at the printed address.
- `startup::requested_port`: an explicit `--port` is bound, and the printed address names it.
- `startup::rejects_unknown_agent`: an id the catalog does not publish exits `1`, names the failure on standard error, and prints nothing on standard output.

## ACP endpoint

The ACP endpoint MUST carry ACP over HTTP/SSE and over WebSocket on the same path. (`endpoint::http_initialize`, `endpoint::sse_responses`, `endpoint::websocket`)

A connection begins with an `initialize` request `POST`ed as JSON, with `content-type: application/json` and no connection header.
It MUST answer `200` with the agent's JSON-RPC response and an `acp-connection-id` header naming the new connection. (`endpoint::http_initialize`)

```text
$ curl -s -X POST http://127.0.0.1:50111/acp -H 'content-type: application/json' -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}}'
{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}
```

The body is the agent's answer, which the transport passes through unchanged and with no trailing newline.
A `POST` without `content-type: application/json` MUST answer `415` with the body `Content-Type must be application/json`. (`endpoint::rejects_missing_content_type`)

After `initialize`, further messages are `POST`ed with the `acp-connection-id` header:

- a message carrying a live connection id MUST answer `202`, and its response arrives on that connection's event stream; (`endpoint::connection_lifecycle`)
- a message with no connection id that is not an `initialize` MUST answer `400` with the body `Acp-Connection-Id header required`; (`endpoint::connection_lifecycle`)
- a message with a connection id the server does not know MUST answer `404`. (`endpoint::connection_lifecycle`)

The connection's event stream is opened by `GET` with `accept: text/event-stream` and the connection id.
It MUST answer `200` with `content-type: text/event-stream`, and each ACP message MUST arrive as a Server-Sent Event carrying that message's JSON in its `data:` field. (`endpoint::sse_responses`)
A `GET` without `accept: text/event-stream` MUST answer `406` with the body `client must accept text/event-stream`. (`endpoint::rejects_missing_accept`)

`DELETE` with the connection id MUST close the connection and answer `202`; deleting the same id again MUST answer `404`, and a `DELETE` with no connection id MUST answer `400` with the body `Acp-Connection-Id header required`. (`endpoint::connection_lifecycle`)

A WebSocket upgrade of the same path MUST answer `101 Switching Protocols`, MUST carry an `acp-connection-id` header, and MUST then carry the same ACP messages as text frames, so a request sent as text is answered with a text frame. (`endpoint::websocket`)

### Tests

- `endpoint::http_initialize`: `initialize` answers `200` with the agent's JSON-RPC result and a non-empty connection id header.
- `endpoint::rejects_missing_content_type`: a `POST` without a JSON content type answers `415` with the content-type message.
- `endpoint::rejects_missing_accept`: a `GET` without the event-stream accept answers `406` with the accept message.
- `endpoint::connection_lifecycle`: a message with the connection id is `202`, without one is `400`, with an unknown id is `404`, and `DELETE` closes (`202`, then `404`) and rejects a missing header (`400`).
- `endpoint::sse_responses`: the event stream answers `200 text/event-stream` and delivers a posted message's response as a `data:` event.
- `endpoint::websocket`: the upgrade answers `101` with a connection id header and carries `initialize` and `test/echo` as text frames.

## Health probe

`GET /health` MUST answer `200` with the body `ok` and no trailing newline. (`health::ok`)
`--no-health` MUST remove the endpoint, so `/health` then answers `404`. (`health::disabled`)

```text
$ curl -s http://127.0.0.1:50111/health
ok
```

### Tests

- `health::ok`: `/health` answers `200` with exactly `ok`.
- `health::disabled`: `--no-health` leaves `/health` answering `404`.

## Readiness probe

`GET /readyz` reports whether agent processes launch.
Before any launch has failed it MUST answer `200` with the body `ready` followed by a newline. (`readiness::ready`)

```text
$ curl -s http://127.0.0.1:50111/readyz
ready
```

After a launch has failed it MUST answer `503` with a body naming how many launches failed and the most recent failure:

```text
not ready: <failures> of <attempts> agent launches failed; last failure (<age> ago): <detail>
```

`<detail>` is that launch's error, which includes the agent process's standard-error tail when the agent started and then failed. (`readiness::failure_detail`)

`--no-readyz` MUST remove the endpoint, so `/readyz` then answers `404` and the startup line omits the readiness address. (`readiness::disabled`)

The readiness probe is served outside the browser-origin layer, so it carries no origin headers even when a policy is set; the ACP endpoint and `/health` are subject to it.

### Tests

- `readiness::ready`: `/readyz` answers `200` with `ready` and a newline before any launch.
- `readiness::failure_detail`: after a launch fails, `/readyz` answers `503` with the failure count and the agent's stderr tail.
- `readiness::disabled`: `--no-readyz` leaves `/readyz` answering `404` and drops the readiness line from standard error.

## Mount paths

`--mount-path <MOUNT>` (also spelled `--subpath`) MUST serve every endpoint under `<MOUNT>`: health becomes `<MOUNT>/health`, and the ACP endpoint becomes `<MOUNT><path>`. (`paths::mount_prefix`)
A request for the same suffix without the prefix MUST answer `404`. (`paths::mount_prefix`)

`--agent-mount-path` (also spelled `--agent-sub-path`) MUST use `/<AGENT_ID>` as the mount prefix, equivalent to `--mount-path /<AGENT_ID>`. (`paths::agent_sub_path`)
It MUST NOT be combined with `--mount-path`, and combining them is rejected with exit status `2`. (`paths::rejects_conflicting_mounts`)

`--path <PATH>` MUST move the ACP endpoint to `<PATH>`, so the default `/acp` then answers `404`. (`paths::custom_endpoint_path`)

A mount prefix that does not start with `/`, is exactly `/`, or ends with `/` MUST fail the command with exit status `1` and a message naming the problem on standard error, before it serves anything. (`paths::rejects_invalid_mount`)
An ACP path that does not start with `/`, is exactly `/`, or collides with an enabled probe MUST fail the same way. (`paths::rejects_invalid_endpoint_path`)

### Tests

- `paths::custom_endpoint_path`: `--path` moves the ACP endpoint, leaves the probes in place, and the default path answers `404` (as does any other path).
- `paths::mount_prefix`: `--subpath` moves health, readiness, and the ACP endpoint (including WebSocket) under the prefix, and the bare paths answer `404`.
- `paths::agent_sub_path`: `--agent-sub-path` mounts every endpoint under `/<AGENT_ID>` and the bare paths answer `404`.
- `paths::rejects_invalid_mount`: a relative, root, or trailing-slash mount exits `1`, names the problem, and prints nothing on standard output.
- `paths::rejects_invalid_endpoint_path`: a relative or root ACP path, or one that collides with an enabled probe, exits `1` with the named problem.
- `paths::rejects_conflicting_mounts`: `--agent-sub-path` with `--subpath` is rejected with exit status `2`.

## Browser origins

No browser origin is allowed by default: a WebSocket upgrade carrying an `Origin` MUST be rejected with `403`, and responses carry no `access-control-allow-origin` header. (`cors::disabled_by_default`)

`--cors-origin <ORIGIN>` may be repeated to allow specific origins.
A cross-origin preflight from a listed origin MUST answer `200` with `access-control-allow-origin` naming it, and a preflight from an unlisted origin MUST answer without that header. (`cors::allowed_origins`)

`--allow-any-origin` MUST allow every origin: an upgrade from any `Origin` succeeds, and responses carry `access-control-allow-origin: *`. (`cors::allow_any_origin`)

`--cors-origin` and `--allow-any-origin` MUST NOT be combined, and combining them is rejected with exit status `2` and a message on standard error. (`cors::rejects_conflicting_options`)

### Tests

- `cors::disabled_by_default`: an upgrade with an `Origin` is rejected `403` and no response carries `access-control-allow-origin`.
- `cors::allowed_origins`: repeated `--cors-origin` values allow each listed origin and leave an unlisted origin without the header.
- `cors::allow_any_origin`: `--allow-any-origin` accepts any upgrade and answers with the wildcard header.
- `cors::rejects_conflicting_options`: `--cors-origin` with `--allow-any-origin` is rejected with exit status `2`.

## Process limit

`--max-processes <N>` bounds how many agent processes one served route runs at once, and its default is `16`.
While the limit is reached, a new initial connection MUST be rejected with `503` and the body `agent process capacity exhausted` followed by a newline, and both probes MUST stay available at `200`. (`limit::rejects_overload`)
A WebSocket upgrade while the limit is reached MUST be rejected with `503` as well. (`limit::rejects_websocket_overload`)
Closing a connection MUST release its slot, so a later initial connection succeeds. (`limit::rejects_overload`)

### Tests

- `limit::rejects_overload`: with `--max-processes 1`, a second initial connection is `503` while health and readiness stay `200`, and closing the first connection frees the slot.
- `limit::rejects_websocket_overload`: with the limit reached, a WebSocket upgrade is rejected `503`.

## Shutdown

`SIGTERM` or `SIGINT` MUST stop the process and MUST exit with status `0`. (`shutdown::termination_signal`)
Connections still open are given a short grace, three seconds in this build, to finish and MUST then be closed, so an event stream held open ends without the client closing it. (`shutdown::closes_active_stream`)

### Tests

- `shutdown::termination_signal`: `SIGTERM` and `SIGINT` each exit the process with status `0`.
- `shutdown::closes_active_stream`: an event stream held open is closed by the server within the grace, and the process exits `0`.

## Out of scope and open questions

Named instances that register the same transports behind `server` routes are [`Server.md`](Server.md)'s subject.
Resolving `<AGENT_ID>` from the catalog, the `--yolo` argument mapping, and the `<ARGS>` passed to the agent are shared with `run` and belong to their own docs.
The ACP message shapes beyond these transports, such as sessions and prompts, are the agent's, not this command's.

- `GET /health` is subject to the browser-origin layer while `GET /readyz` is not; whether that asymmetry is intended is unresolved.
- The drain grace is three seconds in this build; whether that number is part of the published contract is unresolved.
