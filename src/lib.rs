#![doc = r#"
`acp-agent` provides library components behind the `acp-agent` CLI for discovering
ACP agents from the public registry.

## Library Surface

This package primarily exists as a CLI, but the implementation is also exposed as
a library for embedding:

- [`commands`] embeds the CLI parser and dispatch logic.
- [`installer`] installs agent distributions and local toolchains.
- [`registry`] loads and queries the public ACP registry.
- [`runner`] launches registry agents as local processes.
"#]
#![warn(missing_docs)]

/// CLI parsing and command-dispatch helpers used by the `acp-agent` executable.
pub mod commands;
/// Agent distribution and local toolchain installers.
pub mod installer;
/// Types and helpers for loading ACP agent metadata from the public registry.
pub mod registry;
/// Local ACP agent command resolution and execution.
pub mod runner;
