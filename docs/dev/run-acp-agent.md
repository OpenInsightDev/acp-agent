# Running ACP Agents from the Registry

## Context

Registry agents publish via three channels: `npx`, `uvx`, and `binary`.

## Decision

- `binary` / `uvx`: follow upstream — download the binary, or shell out to `uv`.
- `npx`: use Deno's `deno x` instead of `npx` (if `npm` is not present).

Node.js is heavy to install in constrained environments (e.g. containers), so we prefer a modern TS runtime.

Chosen Deno over Bun:
- Deno has higher Node.js API compatibility and is well maintained.
- Bun is controlled by Anthropic and carries political baggage (AI rewrite, Zig, etc.).

## Implementation

`npm_command_spec` in `src/runner.rs` dispatches on toolchain:
- `npm` present → `npm exec -- <package>`
- `npm` missing → `deno x --allow-all --minimum-dependency-age 0 <package>`

Resolution priority in `resolve_agent_command`: `binary` → `npx` → `uvx`.
`src/installer/environment.rs` installs `deno` (JS) and `uv` (Python).
