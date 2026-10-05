# Run

`acp-agent run <id> [--yolo] [-- <args>]` launches one registry agent as a local process with the caller's standard streams attached, and it commits to these outcomes:

- It MUST select the distribution the catalog declares for this host, preferring a current-platform binary target over the agent's `npx` distribution and `npx` over `uvx`. (`resolve::priority`, `resolve::fallback`)
- It MUST fetch and cache a binary distribution on demand, so a binary agent runs without a prior `install`. (`resolve::binary`)
- It MUST launch the agent with the selected distribution's fixed arguments, then the catalog's `args`, then the caller's arguments after `--`, and MUST add the catalog's `env` to the agent's environment. (`args::catalog_and_user`, `args::env`)
- An argument beginning with `-` MUST be placed after `--`; supplied before it, the argument is a usage error. (`args::hyphen_requires_separator`)
- It MUST exit with the agent process's status, mapping a death by signal `<N>` to `128 + <N>`. (`exit::agent_code`, `exit::signal`)
- It MUST attach the caller's standard input, standard output, and standard error to the agent process. (`streams::inherited`)
- An `<id>` the catalog does not carry MUST fail with exit `1` and a message naming the failure on standard error, leaving standard output empty. (`resolve::unknown`)

`--yolo` only changes the arguments the agent launches with; the injection itself is [YOLO.md](YOLO.md)'s subject.

## Resolution

`run` selects one distribution for the agent whose `id` is `<id>`. (`resolve::priority`, `resolve::fallback`)
A binary target the catalog declares for the host platform MUST win over the agent's `npx` distribution, and `npx` MUST win over `uvx`.
A binary target declared only for another platform does not count, so an agent without a host target falls through to its package channels.
The host platform is the running process's operating system and architecture, one of `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64`, or `linux-x86_64`.

The selected distribution decides the launched program and its fixed arguments; a package distribution's program and argument table are [InstallPackage.md](InstallPackage.md)'s subject.
A binary distribution MUST be fetched, extracted, and cached on first use, at the cache path [InstallBinary.md](InstallBinary.md) defines, so `run` of a binary agent needs no prior `install`. (`resolve::binary`)

An `<id>` the catalog does not carry MUST fail the command with exit `1`, with the failure on standard error and empty standard output. (`resolve::unknown`)
Standard error carries `Error: failed to run agent "<id>"` followed by a caused-by chain that names the resolution step and the missing id.

```text
$ acp-agent run not-a-real-agent
Error: failed to run agent "not-a-real-agent"

Caused by:
    0: failed to resolve agent "not-a-real-agent" from registry
    1: agent with id "not-a-real-agent" was not found
```

### Tests

- `resolve::binary`: a binary agent is fetched, cached, and executed in one `run`, with the host archive requested once and no prior install.
- `resolve::priority`: an id declaring a host binary and both package channels runs the binary, and no package manager is invoked.
- `resolve::fallback`: an id declaring a binary only for another platform runs `npx` rather than `uvx`, and the other platform's archive is never fetched.
- `resolve::unknown`: an id absent from the catalog exits `1`, names the failure on standard error, and prints nothing on standard output.

## Arguments and environment

The launched agent's argument list MUST be, in order: the selected distribution's fixed arguments, then the catalog's `args`, then the arguments the caller supplied after `--`. (`args::catalog_and_user`)
For a binary distribution the fixed arguments are empty, so the catalog's `args` come first.
The `--` separator itself MUST NOT be passed to the agent.

Catalog `env` MUST be added to the agent process's environment, which otherwise inherits the caller's. (`args::env`)

An argument beginning with `-` MUST be placed after `--`, where `run` passes it through to the agent.
Supplied directly, such an argument is read as one of `run`'s own options, and MUST fail the command with exit `2`, a usage error naming the argument on standard error, and empty standard output. (`args::hyphen_requires_separator`)

```text
$ acp-agent run mock-npx --extra
error: unexpected argument '--extra' found

  tip: to pass '--extra' as a value, use '-- --extra'

Usage: acp-agent run <AGENT_ID> [ARGS]...

For more information, try '--help'.
```

### Tests

- `args::catalog_and_user`: the catalog `args` precede the caller's arguments after `--`.
- `args::hyphen_requires_separator`: a hyphen-prefixed argument before `--` exits `2` with the usage error and no standard output.
- `args::env`: the catalog-declared environment reaches the agent process.

## Exit status

`run` MUST exit with the agent process's status: a normal exit `<N>` becomes status `<N>`, and a death by signal `<N>` becomes status `128 + <N>`. (`exit::agent_code`, `exit::signal`)
On success it MUST exit `0`. (`resolve::binary`)
It adds no output of its own to either standard stream. (`streams::inherited`)

### Tests

- `exit::agent_code`: an agent that exits `7` makes `run` exit `7`.
- `exit::signal`: an agent killed by `SIGTERM` or `SIGKILL` makes `run` exit `143` or `137`.

## Streams

`run` MUST attach the caller's standard input, standard output, and standard error to the agent process, so the agent reads the caller's input directly and its output and errors reach the caller's streams unchanged. (`streams::inherited`)

### Tests

- `streams::inherited`: input written to `run` reaches the agent, and the agent's standard output and error reach `run`'s own with nothing added.

## Out of scope

`--yolo` argument injection belongs to [YOLO.md](YOLO.md).
The package runners' programs, their argument tables, and the install/run runner decision are [InstallPackage.md](InstallPackage.md)'s subject.
Fetching, decoding, and validating the catalog, and the shape of `distribution`, are [Registry.md](Registry.md)'s subject.
The binary cache layout, checksums, and the `install`, `update`, and `uninstall` commands are [InstallBinary.md](InstallBinary.md)'s subject.
`serve`, the daemon, and the named instances they own are their own specs.

## Open questions

Catalog `env` is merged into the environment the agent inherits, so an agent-declared variable overrides an inherited one of the same name; confirm that override is intended.
A binary agent is launched from its extracted directory while a package agent inherits the caller's working directory; confirm that difference is intended.
