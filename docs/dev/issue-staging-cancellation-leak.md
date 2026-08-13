# Issue: Staging directories leak when installation is cancelled

## Severity

P1/P2

## Status

Resolved in `src/installer/binary.rs` and `src/main.rs`.

## Evidence

`packages/acp-agent/src/installer/binary.rs:88-102` creates a hand-named staging directory and only cleans it on explicit errors.

## Problem

Cancellation during download, extraction, or metadata writing skips the explicit cleanup branches. PID/time-based names remain in the persistent cache after cancellation or crash.

## Impact

Long-running services accumulate downloaded archives and extracted payloads indefinitely.

## Fix

- `cache_binary_target_in_mode` stages the new cache in a `tempfile::Builder::tempdir_in` guard and renames it into the stable cache directory only after `prepare_staging_directory` succeeds.
- The guard's `Drop` removes the staging directory on any error, panic, or cancellation, so an interrupted install never leaves a work directory behind.
- Extraction is cooperative: a `CancellableExtraction` future sets an `AtomicBool` flag when the awaiting install future is dropped, and `extract_tar`/`extract_zip` check it between entries.
- The detached `spawn_blocking` extraction therefore stops before the guard removes the directory instead of racing it and resurrecting entries.
- `clean_stale_staging_entries` is invoked at process startup in `main.rs` and removes every dot-prefixed work directory (staging and backup) under the cache's agents tree as a recovery measure for crashes where no guard ever ran.
- ZIP extraction is now per-entry (`enclosed_name` traversal guard, symlink handling, deferred unix mode pass) so cancellation checks can run between entries; existing behavior tests cover directories, files, traversal rejection, symlinks, and permissions.

## Tests

- `cancelled_download_removes_the_staging_directory` aborts an install while the HTTP download is still streaming and asserts the staging directory is gone.
- `cancelled_extraction_removes_the_staging_directory` aborts an install mid-extraction of a 30k-entry ZIP and asserts the staging directory is gone.
- `startup_sweep_removes_stale_staging_and_backup_directories` plants abandoned work directories and asserts the startup sweep removes them while keeping completed cache entries.
