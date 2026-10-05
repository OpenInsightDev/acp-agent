#!/bin/sh
#
# Package a built binary as a release asset.
#
# Usage: package-release.sh <binary-path> <asset-suffix>
#
# `<bin>-<os>-<arch>.tar.gz` is the naming contract with scripts/install.sh, and
# carries no version so installers can use the releases/latest/download/
# redirect while old versions stay reachable via releases/download/<tag>/.
set -eu

BIN_PATH="$1"
ASSET_SUFFIX="$2"
BIN_NAME="$(basename "$BIN_PATH")"

mkdir -p dist/data
cp "$BIN_PATH" "dist/$BIN_NAME"
cp data/yolo-modes.json dist/data/yolo-modes.json
tar -czf "dist/${BIN_NAME}-${ASSET_SUFFIX}.tar.gz" -C dist "$BIN_NAME" data/yolo-modes.json
rm "dist/$BIN_NAME"
