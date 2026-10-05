# Daemon

`acp-agent daemon` is the foreground process that owns named ACP instances and serves the control protocol on one user-scoped Unix socket, and it takes no options.
It commits to these outcomes, in this order:

- It MUST bind one socket path, resolved by the rules in [Socket path](#socket-path). (`socket::rejects_invalid_override`)
- It MUST allow only one live daemon on that socket. (`ownership::single_owner`)
- It MUST recover a socket a dead daemon left behind, and MUST NOT remove an unrelated filesystem entry at that path. (`recovery::stale_socket`)
- It MUST answer a health request with the control-protocol version it implements. (`control::health`)
- `SIGTERM` or `SIGINT` MUST stop it, removing its socket and exiting `0`. (`lifecycle::termination_signal`)

## Socket path

`ACP_AGENT_DAEMON_SOCKET` names the socket when it is set; with it unset the daemon MUST bind `<user-scoped runtime directory>/acp-agent/daemon/daemon.sock`. (`socket::default_path`)
An override MUST be an absolute path that names a socket file and MUST NOT be longer than 103 bytes, the length a Unix socket address can hold.
An override that is empty, relative, names no file, or is too long MUST fail the daemon before it binds, with exit status `1`, a message naming the problem on standard error, and empty standard output. (`socket::rejects_invalid_override`)
The parent directory MUST have mode `0700`; a missing parent MUST be created with that mode, and a parent with any other mode MUST fail the daemon the same way before it binds. (`socket::creates_private_parent`, `socket::rejects_public_parent`)

```text
$ ACP_AGENT_DAEMON_SOCKET= acp-agent daemon
Error: ACP_AGENT_DAEMON_SOCKET must not be empty
```

### Tests

- `socket::rejects_invalid_override`: an empty, relative, file-less, or too-long override each fails with exit `1` and a message naming the problem, and no socket is created.
- `socket::rejects_public_parent`: an existing parent directory with a mode other than `0700` fails the daemon the same way.
- `socket::creates_private_parent`: a socket under a missing parent binds, and the created parent has mode `0700`.
- `socket::default_path`: with the override unset the daemon binds at the user-scoped default path.

## Single ownership

One daemon owns a socket.
A second daemon started while the first is live MUST fail to bind, with exit status `1`, a message on standard error, and empty standard output, and the first daemon and its socket MUST stay unchanged. (`ownership::single_owner`)

### Tests

- `ownership::single_owner`: a second daemon on a live socket fails and the first daemon keeps serving.

## Stale-socket recovery

A socket file left behind by a daemon that died without cleaning up MUST be removed and rebound, so a new daemon recovers the endpoint. (`recovery::stale_socket`)
Any other filesystem entry at the socket path, a regular file or a directory, MUST be reported as already in use, with exit status `1` and a message on standard error, and MUST NOT be removed. (`recovery::unrelated_entry`)

### Tests

- `recovery::stale_socket`: a socket left by a killed daemon is removed and a new daemon binds it.
- `recovery::unrelated_entry`: a regular file or a directory at the socket path fails the daemon and stays in place.

## Health handshake

The daemon MUST serve the versioned control protocol on its socket.
A health request MUST answer with the protocol version the daemon implements, which is `2`. (`control::health`)

### Tests

- `control::health`: a health request over the socket answers with protocol version `2`.

## Shutdown

`SIGTERM` or `SIGINT` MUST stop the daemon.
The daemon MUST remove its socket and MUST exit with status `0`. (`lifecycle::termination_signal`)

### Tests

- `lifecycle::termination_signal`: `SIGTERM` and `SIGINT` each exit the daemon with status `0` and remove its socket.

## Out of scope

Named instances, route registration, and the `server` client commands that drive a daemon are specified by [Server.md](Server.md).
The control protocol's wire framing and its commands other than health are out of this spec.

## Open questions

The default socket location follows the operating system's user-scoped runtime directory, so it differs between platforms; confirm the intended location on each supported platform.
Binding also leaves a lock file beside the socket; confirm whether that file is part of the published contract or an implementation detail.

