#![doc = r#"
Client and library building blocks for installing, discovering, launching, and proxying
ACP agents from the public registry.

This crate powers the `acp-agent` CLI, but its modules are also exposed for consumers that
want to integrate ACP agent discovery or process orchestration into their own tools.

The main entry points are:

- [`commands`] for embedding the CLI parser and dispatch logic.
- [`registry`] for decoding and querying the published registry metadata.
- [`runtime`] for turning registry metadata into runnable child processes or network transports.
"#]
#![warn(missing_docs)]

/// CLI parsing and command-dispatch helpers used by the `acp-agent` executable.
pub mod commands;
/// Types and helpers for loading ACP agent metadata from the public registry.
pub mod registry;
/// Runtime primitives for preparing agent commands and exposing them over transports.
pub mod runtime;
