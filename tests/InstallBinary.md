# Install Binary

This document specifies how `acp-agent` installs, updates, and removes an agent that the registry publishes as a per-platform binary.
The document commits to these outcomes:

- `install` selects the target the catalog declares for the host platform and publishes it under a digest-keyed cache directory.
- `list --installed` reports the cached entries as TSV and JSON.
- Repeating an install of the same digest reuses the cached entry and does not download the archive again.
- An archive whose bytes do not match the catalog checksum MUST fail without publishing anything.
- An archive entry that would escape the extraction root MUST be rejected.
- An archive with more entries than the configured limit MUST be rejected.
- `update` publishes the catalog's current target as a new cache entry.
- `uninstall` removes the id's cached entries and MUST fail for an id that is not installed.
- Startup recovery sweeps abandoned work directories and restores a backup when its entry is missing.

Catalog resolution, and the shape of the `distribution` object, are [`Registry.md`](Registry.md)'s subject.
Installing and removing agents through npm or uvx is a separate spec and is not covered here.

## install

`acp-agent install <id>` MUST resolve `<id>` from the catalog and publish the target the catalog declares for the host platform. (`install::selects_host_target`)
The host platform is the running process's operating system and architecture, one of `darwin-aarch64`, `darwin-x86_64`, `linux-aarch64`, or `linux-x86_64`.
Only that platform's target is downloaded.

The distribution MUST be published as:

```text
<cache root>/agents/<id>/<platform>/<version>-sha256-<digest>/
  metadata.json
  extracted/<cmd>
```

`<digest>` is the catalog `sha256` in lowercase.
`<cmd>` is the catalog `cmd`, resolved inside `extracted/`.
The manifest `metadata.json` describes the distribution it was built from — agent, version, platform, archive URL, command, and archive checksum — plus the digests that later prove the extracted payload unchanged.
The published executable is given the owner-execute permission, whatever mode the archive carried. (`install::publishes_digest_keyed_cache`)

A successful install MUST print `Installed <id> binary at <executable> (cache: <cache_dir>)` on standard output and MUST exit `0`.

The examples here and below quote a capture from macOS on Apple Silicon, so the cache root, the platform key, and the digest in them differ on another host.

```text
$ acp-agent install mock-binary
Installed mock-binary binary at /tmp/acp-binary-example/home/Library/Caches/acp-agent/agents/mock-binary/darwin-aarch64/2.3.4-sha256-1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a/extracted/bin/mock-binary (cache: /tmp/acp-binary-example/home/Library/Caches/acp-agent/agents/mock-binary/darwin-aarch64/2.3.4-sha256-1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a)
```

When the digest-keyed entry already exists and validates, install MUST reuse it and MUST NOT download the archive again. (`install::warm_cache_reuses_archive`)

### Tests

- `install::selects_host_target`: a catalog declaring all four platform targets yields only the host platform's entry, whose manifest names the host platform's archive URL.
- `install::publishes_digest_keyed_cache`: the entry is `<version>-sha256-<digest>` with a manifest and a runnable `extracted/<cmd>`, and the command prints that executable's path.
- `install::warm_cache_reuses_archive`: a second install of the same digest reuses the entry and re-downloads nothing.
- `install::lists_installed_agents`: an installed binary yields the record fields and the matching TSV columns.

## list --installed

`acp-agent list --installed [--json]` MUST report the local cache and MUST exit `0`.
`--json` prints the records as a pretty-printed JSON array, each object carrying these fields:

| Field | Presence | Value |
| --- | --- | --- |
| `id` | always | Registry id of the cached agent. |
| `version` | always | Cached agent version. |
| `platform` | always | Platform cache key the entry lives under. |
| `cache_dir` | always | Directory that owns the extracted payload. |
| `executable_path` | always | Executable entry point inside the extracted payload. |

Every field is always present, and none is printed as `null`.
The default TSV form MUST print one record per line with four tab-separated columns in this order: `id`, `version`, `platform`, `cache_dir`.
There is no header line, values are printed verbatim, and the last line ends with a newline; an empty cache prints nothing.
Records are ordered by `id`, then `version`, then `platform`.

```text
$ acp-agent list --installed
mock-binary	2.3.4	darwin-aarch64	/tmp/acp-binary-example/home/Library/Caches/acp-agent/agents/mock-binary/darwin-aarch64/2.3.4-sha256-1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a
```

```json
$ acp-agent list --installed --json
[
  {
    "id": "mock-binary",
    "version": "2.3.4",
    "platform": "darwin-aarch64",
    "cache_dir": "/tmp/acp-binary-example/home/Library/Caches/acp-agent/agents/mock-binary/darwin-aarch64/2.3.4-sha256-1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a",
    "executable_path": "/tmp/acp-binary-example/home/Library/Caches/acp-agent/agents/mock-binary/darwin-aarch64/2.3.4-sha256-1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a/extracted/bin/mock-binary"
  }
]
```

### Tests

- `install::lists_installed_agents`: an installed binary yields one JSON record with the five fields and the matching four-column TSV line.

## verify

Before extraction, the downloaded bytes MUST be hashed and compared with the catalog `sha256`.
A mismatch MUST fail the install with `sha256 checksum mismatch` on standard output, MUST exit `1`, MUST publish no cache entry, and MUST leave no work directory behind. (`verify::rejects_checksum_mismatch`)
For a failed install, standard output carries the failure line and nothing else, and standard error stays empty.

```text
$ acp-agent install mock-binary
failed to install agent "mock-binary": sha256 checksum mismatch: expected 1deb43248a2f90506e3ee51234c26263beaed8474780414100ea489df52b868a, got 56d6a55bac127af97bd00a443e3c7b94bf1ea67fe056602876aa9c7632f41f8c
```

### Tests

- `verify::rejects_checksum_mismatch`: bytes that do not match the catalog checksum fail the install and leave neither a cache entry nor a work directory.

## archive

Extraction MUST refuse any archive entry whose path would escape the extraction root.
Such an install MUST fail with `unsafe archive path` on standard output, MUST exit `1`, MUST publish no cache entry, and MUST leave no work directory behind. (`archive::rejects_path_traversal`)

Extraction MUST enforce the configured archive entry limit.
An archive over it MUST fail with `archive entry limit exceeded` on standard output, MUST exit `1`, MUST publish no cache entry, and MUST leave no work directory behind. (`archive::rejects_entry_limit`)

### Tests

- `archive::rejects_path_traversal`: an archive carrying a `../` entry is refused and nothing is published.
- `archive::rejects_entry_limit`: an archive with more entries than the limit is refused and nothing is published.

## update

`acp-agent update <id>` MUST resolve the current catalog target and publish it exactly as [`install`](#install) does, selecting a new digest-keyed directory when the digest changed. (`update::installs_replacement`)
It MUST then remove the entries it replaced for that agent and platform, so only the current digest remains. (`update::removes_replaced`)
An entry a running server is still executing MUST be left in place and removed by a later update or uninstall.
A successful update MUST print the same message as `install` and MUST exit `0`.

### Tests

- `update::installs_replacement`: the catalog's new digest is published as a new entry with its executable.
- `update::removes_replaced`: after the update the replaced digest's entry is gone and only the new digest remains.

## uninstall

`acp-agent uninstall <id>` MUST remove every cached entry for `<id>`, across platforms and versions, MUST print `Uninstalled <id> from the local cache` on standard output, and MUST exit `0`. (`uninstall::removes_cached_entry`)
A cached entry is removed even when the registry is unreachable, and the command then warns on standard error that package distributions were not checked.

```text
$ acp-agent uninstall mock-binary
Uninstalled mock-binary from the local cache
```

An id that has neither a cached entry nor a package distribution MUST fail with `agent "<id>" is not installed` on standard output and MUST exit `1`. (`uninstall::rejects_unknown_agent`)
Removing package distributions is outside this document.

### Tests

- `uninstall::removes_cached_entry`: the cached entry is gone afterwards and `list --installed` reports nothing.
- `uninstall::rejects_unknown_agent`: an id that is not installed fails with the not-installed error and a non-zero exit.

## recovery

Before running any command, `acp-agent` MUST sweep the cache for work directories left by an install that was interrupted.
A work directory is a dot-prefixed sibling of a digest-keyed entry, named `.<key>-staging-...` or `.<key>-backup-...`, where `<key>` is the entry it belongs to.
The sweep is best effort and MUST NOT fail the requested command.

A stale staging directory MUST be deleted. (`recovery::sweeps_stale_staging`)
When the entry `<key>` is missing and a `-backup-` directory exists, the backup MUST be restored as that entry before the remaining work directories are deleted. (`recovery::restores_backup`)

### Tests

- `recovery::sweeps_stale_staging`: a stale staging directory is deleted at startup and a valid entry beside it survives.
- `recovery::restores_backup`: a backup directory becomes the cache entry when that entry was missing.

## Out of scope and open questions

The install log written to the cache root, and the way `run` and `serve` consume cached binaries, are not specified here.
The registry payload and how `distribution` selects a channel are [`Registry.md`](Registry.md)'s subject.

- The registry URL can be overridden by an environment variable that exists for tests; whether that override should be documented publicly or kept internal is unresolved.
