# CI/CD

## Context

A release publishes the crate on crates.io, the GitHub release assets, and two
container images on GHCR: a Debian runtime image and a binary-only carrier
image.
`cargo release` tags `v<version>`, and that tag is the only release entry point.

## Decision

**One binary per target, built where it runs.**
Images carry the binary from the release job instead of compiling in the
Dockerfile, so the same bytes reach the tarball and the image.
Linux binaries are built in a `rust:1.98-bullseye` container, which holds the
glibc floor at 2.31 so one binary covers Debian 11, Ubuntu 20.04, and newer.
Each architecture builds on a runner of that architecture and is pushed to GHCR
as a digest; only the merged manifest list is tagged, so no tag names a single
architecture.
No step relies on emulation.

**The GitHub release is signed keylessly.**
`sha256sum` covers every asset and `cosign sign-blob` signs that file with the
workflow's OIDC identity, so there is no key to keep or rotate.

**CI checks and tests on both shipped platforms**, because the CLI ships macOS
and Linux binaries and its harness exercises per-OS cache and process behaviour.

## Notes

Asset names (`<bin>-<os>-<arch>.tar.gz`) are a contract between
`scripts/package-release.sh`, the release workflow, and `scripts/install.sh`.

The `rust:1.98-bullseye` reference lives in the workflow, where dependabot cannot
see it, so it is bumped by hand.
