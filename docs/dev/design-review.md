# Design Review Findings

This document records the current implementation review for over-design, unnecessary reimplementation, and avoidable maintenance complexity.

The process-lifecycle finding has been applied as a destructive change.

## Priority Summary

1. Completed: Remove process-tree management and keep cancellation scoped to direct child processes on Unix.
2. Completed: Remove unsupported YOLO protocol modes from the active model.
3. Medium: Replace whitespace splitting for multi-token YOLO flags.
4. Medium: Share registry snapshots across batch operations.
5. Completed: Consolidate repeated route configuration and distinguish mount prefixes from public route prefixes.
6. Completed: Centralize distribution priority and remove duplicate runner enums.
7. Low: Narrow `ArchiveLimits` visibility and consider standard codecs/concurrency helpers only when their additional semantics are needed.

## Findings

### 1. Custom Process-Tree Management

**Priority:** High.

**Locations:** `src/process.rs` and `src/server/client.rs` process startup cleanup.

The process-tree abstraction has been removed completely.

Local command cancellation now uses Tokio's `kill_on_drop` for the direct child only; no process groups, PID signaling, unsafe code, or descendant cleanup are part of the design.

Daemon startup retains a small direct-child guard so a failed or cancelled startup reaps the daemon child before returning.

The crate is explicitly Unix-only, so no non-Unix lifecycle fallback is maintained.

### 2. YOLO Models Include Unsupported Protocol-Level Modes

**Priority:** Medium.

**Status:** Completed.

**Locations:** `src/yolo.rs` and `data/yolo-modes.json`.

`YoloModeInfo` now models only the startup CLI flag injected by `--yolo`. The unsupported `mode` and `option` fields, `YoloConfigOption`, `has_no_yolo`, and the protocol-level error branches have been removed.

The catalog uses `deny_unknown_fields`, so ACP `session/set_mode` and `session/set_config_option` entries are rejected instead of being silently accepted as future-compatible data. Entries without a supported CLI flag are not included in the catalog.

### 3. YOLO Arguments Are Parsed with `split_whitespace`

**Priority:** Medium.

**Location:** `src/yolo.rs:161-177`.

The current parser splits a catalog string with `split_whitespace`.

This cannot preserve quoted or escaped argument values such as `--prompt "allow all tools"`.

The parser can therefore produce incorrect process arguments for valid shell-like catalog entries.

**Recommendation:** Prefer an array schema such as `{ "args": ["--prompt", "allow all tools"] }`.

Use a dedicated shell-token parser only as a compatibility layer for the existing string schema, and never execute catalog values through a shell.

### 4. `stale_cache_entries_removed` Represents an Unreachable Production State

**Priority:** Medium.

**Locations:** `src/installer/agents.rs:39-52`, `src/installer/agents.rs:201-210`, `src/installer/agents.rs:259-267`, and `src/commands/agents.rs` warning handling.

`InstallOutcome::Binary` exposes `stale_cache_entries_removed`, but current production constructors always set it to `false`.

The implementation explicitly defers cache cleanup to a future garbage-collection operation.

The field nevertheless propagates through the result model and CLI warning logic.

This is an unsupported future state embedded in every binary installation result.

**Recommendation:** Remove the field and its warning branch until a real cache garbage-collection operation exists.

A future garbage-collection command can return its own cleanup result without expanding every install result.

### 5. Route Configuration Is Repeated Across Layers

**Priority:** Medium.

**Status:** Completed.

**Locations:** `src/serve.rs`, `src/commands/mod.rs`, `src/server/protocol.rs`, and `src/server/mod.rs`.

The same route options are represented separately by standalone serve CLI fields, server-register CLI fields, wire protocol data, `RegisterOptions`, and service-layer options.

The conversions are spread across multiple command, client, and daemon functions.

Adding a new route option requires updating several structures and manual mappings, which creates configuration drift risk.

The standalone `serve --subpath` and `server register --subpath` compatibility alias also represent different routing concepts.

`RouteConfig` is now the shared internal route configuration for standalone serving and named registrations. It owns the ACP path, CORS inputs, health/readiness flags, and process limit. The CLI and daemon wire request remain parsing/serialization boundaries, while `RegisterOptions` and `RouteId` carry the shared configuration without repeating its fields.

Standalone serving calls its prefix `mount_path` internally and accepts the legacy `--subpath` alias. Named server registration calls its public prefix `route` and no longer aliases it as `--subpath`.

### 6. Distribution Priority Is Reimplemented in Several Operations

**Priority:** Medium.

**Status:** Completed.

**Locations:** `src/runner.rs` and `src/installer/agents.rs`.

**Locations:** `src/runner.rs:151-187` and `src/installer/agents.rs:169-257`.

Run, install, update, and uninstall each encode parts of the binary, npm, and uvx distribution priority.

A new distribution type or priority change could therefore produce inconsistent behavior across operations.

**Recommendation:** Resolve a registry agent once into a typed distribution such as `ResolvedDistribution::Binary`, `ResolvedDistribution::Npm`, or `ResolvedDistribution::Uvx`.

`runner::resolve_distribution` now returns that typed result using the shared binary, npm/Deno, then uvx priority. Run, install, update, and uninstall consume the result; `PackageRunner` is also the single reporting/install strategy type, with `install_name()` covering the display-only distinction between `uvx` and `uv`.

### 7. Batch Operations Fetch the Registry Repeatedly

**Priority:** Medium.

**Status:** Completed.

**Locations:** `src/installer/agents.rs:134-236`.

Batch install, update, and uninstall now fetch one registry snapshot at the batch boundary and share it across all distinct agent operations. This limits each batch to one registry request and ensures every operation uses the same catalog data.

Registry-fetch failures are reported for each requested agent without starting distribution work. Uninstall preserves the existing fallback: if cached binaries were removed, it still succeeds and records the registry error so package-manager distributions can be inspected later.

### 8. Cache Lock Acquisition Contains Mechanical Duplication

**Priority:** Medium.

**Locations:** `src/installer/cache.rs:63-244`.

The blocking worker, readiness channel, release channel, guard lifetime, and error handling are repeated by exclusive, shared, write-use, and try-lock functions.

These lock modes have different correctness purposes and should not be collapsed into one generic mutex abstraction.

The surrounding acquisition protocol can nevertheless be centralized behind an internal helper parameterized by lock mode and try-lock behavior.

**Recommendation:** Extract the common async-to-blocking lock bridge while preserving the distinct publish, read-lease, write-lease, and cleanup semantics.

### 9. `PackageRunner` and `InstallMethod` Duplicate the Same Enum

**Priority:** Low.

**Status:** Completed.

**Locations:** `src/runner.rs:31-52` and `src/installer/agents.rs:26-35`.

Both enums contain `Npm`, `Deno`, and `Uvx`, and installation converts one into the other with a manual match.

The duplicate types require synchronized changes and duplicate display or formatting logic.

`InstallMethod` has been removed. Installation and uninstall outcomes retain `PackageRunner`, whose `install_name()` supplies the package-manager display name where it differs from the executable name.

`ArchiveLimits` remains a separate follow-up from the requested issues.

### 10. `ArchiveLimits` Is Public Without a Public Configuration Path

**Priority:** Low.

**Locations:** `src/installer/binary.rs:44-75` and the download/extraction entry points around `src/installer/binary.rs:534-677`.

`ArchiveLimits` is publicly visible, but production installation uses the default value and does not expose a public API that accepts custom limits.

This suggests configurability that library users cannot actually use and unnecessarily expands the public API surface.

**Recommendation:** Make the type private or `pub(crate)` until a complete public installation configuration API is provided.

### 11. Handwritten Protocol Framing Is Replaceable but Not Urgent

**Priority:** Low.

**Location:** `src/server/protocol.rs:174-225`.

The daemon protocol manually implements four-byte length framing, frame-size checks, exact reads and writes, and JSON serialization boundaries.

`tokio_util::codec::LengthDelimitedCodec` could provide the framing layer while `serde_json` remains responsible for the payload.

The current implementation is small, explicit, and tested, so replacing it only to remove a few dozen lines has limited value.

**Recommendation:** Revisit this if the protocol evolves toward multiple requests per connection or a streaming `Framed` implementation.

### 12. Batch Concurrency Infrastructure Could Use Existing Futures Utilities

**Priority:** Low.

**Location:** `src/installer/agents.rs:409`.

The custom `run_concurrently` helper combines deduplication, a semaphore, spawned tasks, panic handling, result collection, and input-order restoration.

The existing `futures` dependency could express part of this with `buffer_unordered` or `FuturesUnordered`.

A replacement must preserve the current panic isolation, cancellation, deduplication, and result-order semantics.

**Recommendation:** Treat this as a maintainability refactor only, and do not replace it without focused regression tests.

### 13. Handwritten Hex Encoding Is a Low-Value Replacement Candidate

**Priority:** Low.

**Location:** `src/installer/binary.rs:644-657`.

The SHA-256 parsing and hexadecimal encoding helpers could use the established `hex` crate.

The current implementation is small, and adding a dependency would provide limited benefit by itself.

**Recommendation:** Consider this only if the project adopts a shared hexadecimal utility elsewhere.

## Complexity That Should Be Preserved

The following areas are complex for concrete correctness or safety reasons and should not be simplified solely to reduce line count.

### Binary Cache Publication

Staging, backup, atomic promotion, rollback, cancellation handling, and stale-staging recovery protect downloads and running processes across failure boundaries.

These states should be split into smaller modules only if the behavior and tests remain unchanged.

### Cache Locks and Payload Leases

Publish locks, payload read leases, payload write leases, and non-blocking cleanup locks protect different operations.

They should not be replaced with one undifferentiated lock.

### Launch State and Readiness Generations

Generation tracking prevents an older connection from overwriting newer readiness state during concurrent launches.

Exactly-once outcome recording and stderr retention also represent real lifecycle requirements.

### WebSocket Admission Reservations

The reservation mechanism accounts for the interval between an HTTP upgrade and the actual creation of an ACP agent process.

A plain semaphore would not necessarily enforce the intended relationship between accepted connections and active agent processes.

### Named Server Protocol Layers

The CLI, client, Unix-socket framing, daemon handler, and route runtime have separate responsibilities and form a coherent control path.

The protocol layer should be simplified only when its compatibility and frame-size guarantees remain intact.

## Suggested Follow-Up Order

1. Completed: Remove process-tree management and define direct-child cancellation as the Unix-only platform contract.
2. Completed: Remove unreachable YOLO protocol fields from the active model and catalog.
3. Replace whitespace splitting for multi-token YOLO flags.
4. Remove `stale_cache_entries_removed`.
5. Completed: Share registry snapshots and centralize distribution resolution.
6. Consolidate route configuration and cache-lock plumbing.
7. Merge the duplicate runner enums and narrow `ArchiveLimits` visibility.
8. Defer codec, concurrency-helper, and hex utility replacements until they solve a concrete maintenance problem.
