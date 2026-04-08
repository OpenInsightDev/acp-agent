#![doc = r#"
`acp-agent` provides library components behind the `acp-agent` CLI for discovering,
installing, launching, and proxying ACP agents from the public registry.

## Library Surface

This package primarily exists as a CLI, but the implementation is also exposed as
a library for embedding:

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
