#!/bin/sh
#
# Install acp-agent.
#
# Binaries are published as npm platform packages (cargo-npm) and served
# from the jsDelivr CDN, so the bare binary is fetched from
#   https://cdn.jsdelivr.net/npm/@open-insight/acp-agent-<platform>@<version>/acp-agent
# If the CDN is unreachable, the script falls back to the GitHub release
# archive (verified against the SHA256SUMS published with the release).
#
# No GitHub API call is made. The version to install is resolved from
# ACP_AGENT_VERSION, or - for the default "latest" install - by following
# the deterministic releases/latest/download/ redirect once (a single HEAD
# request) to learn the current release tag. The CDN is only ever queried
# with that exact version: `@latest` is not used because it silently serves
# the newest *published npm* version, which can lag behind the GitHub
# release if the npm publish step fails or is slow. Downloaded CDN binaries
# are additionally checked to report the expected version.
#
# Optional env overrides:
#   ACP_AGENT_REPO          GitHub repo (default: OpenInsightDev/acp-agent)
#   ACP_AGENT_BIN_NAME      installed binary name (default: acp-agent)
#   ACP_AGENT_VERSION       version to install, e.g. "0.0.3" (default: latest)
#   ACP_AGENT_INSTALL_DIR   install directory (default: ~/.local/bin)
set -eu

REPO="${ACP_AGENT_REPO:-OpenInsightDev/acp-agent}"
BIN_NAME="${ACP_AGENT_BIN_NAME:-acp-agent}"
INSTALL_DIR="${ACP_AGENT_INSTALL_DIR:-${HOME:-}/.local/bin}"

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}
need_cmd curl
need_cmd install
need_cmd mktemp

OS="$(uname -s)"
ARCH="$(uname -m)"
case "$OS" in
  Darwin) OS=darwin ;;
  Linux) OS=linux ;;
  *) echo "unsupported operating system: $OS" >&2; exit 1 ;;
esac
case "$ARCH" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64|amd64) ARCH=x86_64 ;;
  *) echo "unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Asset naming convention used by .github/workflows/release.yml.
ASSET="${BIN_NAME}-${OS}-${ARCH}.tar.gz"

# npm platform package name for this OS/arch (cargo-npm naming), e.g.
# @open-insight/acp-agent-linux-x64. The bare binary lives at the package root.
case "$OS-$ARCH" in
  linux-x86_64) NPM_PLATFORM="linux-x64" ;;
  linux-aarch64) NPM_PLATFORM="linux-arm64" ;;
  darwin-x86_64) NPM_PLATFORM="darwin-x64" ;;
  darwin-aarch64) NPM_PLATFORM="darwin-arm64" ;;
  *) NPM_PLATFORM="" ;;
esac

# Resolve the exact version to install. Prefer ACP_AGENT_VERSION; otherwise
# follow the releases/latest/download/ redirect (one HEAD request, no API).
# Prints the version, or nothing if it cannot be determined (e.g. GitHub
# unreachable); callers fall back to the plain latest URLs in that case.
resolve_latest_version() {
  # GitHub redirects latest -> releases/download/vX.y.z/... -> signed asset
  # URL, so follow only the first hop and read its Location header.
  curl -sSI --retry 2 --proto '=https' --tlsv1.2 \
    -H "User-Agent: ${BIN_NAME}-install" \
    "https://github.com/$REPO/releases/latest/download/$ASSET" 2>/dev/null |
    tr -d '\r' | sed -n 's/^[Ll]ocation: //p' | head -n 1 |
    sed -n 's#.*/releases/download/v\([^/]*\)/.*#\1#p'
}

if [ -n "${ACP_AGENT_VERSION:-}" ]; then
  VERSION="${ACP_AGENT_VERSION#v}"
else
  VERSION="$(resolve_latest_version)"
fi

download() {
  curl -fsSL --retry 3 --proto '=https' --tlsv1.2 \
    -H "User-Agent: ${BIN_NAME}-install" -o "$2" "$1"
}

# True if the binary runs and reports the expected version, e.g.
# `acp-agent 0.0.4` -> "0.0.4". Also guards against truncated or
# wrong-architecture downloads.
binary_version_matches() {
  [ "$("$1" --version 2>/dev/null | sed 's/.* //')" = "$2" ]
}

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT INT TERM

# 1. Prefer the jsDelivr CDN: the npm platform package carries the bare
# binary, so no archive extraction is needed. Only exact versions are used
# and the downloaded binary must report that version - anything else falls
# back to GitHub.
if [ -n "$NPM_PLATFORM" ] && [ -n "$VERSION" ]; then
  BIN_PATH="$TMP_DIR/$BIN_NAME"
  NPM_BIN_URL="https://cdn.jsdelivr.net/npm/@open-insight/acp-agent-${NPM_PLATFORM}@${VERSION}/acp-agent"
  if download "$NPM_BIN_URL" "$BIN_PATH"; then
    chmod +x "$BIN_PATH" # curl -o does not set the executable bit
    if binary_version_matches "$BIN_PATH" "$VERSION"; then
      mkdir -p "$INSTALL_DIR"
      install -m 0755 "$BIN_PATH" "$INSTALL_DIR/$BIN_NAME"
      echo "installed $BIN_NAME $VERSION to $INSTALL_DIR/$BIN_NAME"
      exit 0
    fi
  fi
  echo "warning: jsDelivr download failed or version mismatch ($NPM_BIN_URL); falling back to GitHub releases" >&2
fi

# 2. Fallback: GitHub release archive, verified against SHA256SUMS.
if [ -n "$VERSION" ]; then
  BASE_URL="https://github.com/$REPO/releases/download/v$VERSION"
else
  BASE_URL="https://github.com/$REPO/releases/latest/download"
fi
ARCHIVE="$TMP_DIR/$ASSET"
download "$BASE_URL/$ASSET" "$ARCHIVE" || {
  echo "failed to download $BASE_URL/$ASSET" >&2
  echo "make sure a release exists for $REPO and this platform" >&2
  exit 1
}

# Verify the archive against the SHA256SUMS published with the release.
if command -v sha256sum >/dev/null 2>&1; then
  CHECK="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
  CHECK="shasum -a 256" # macOS
else
  CHECK=""
  echo "warning: sha256sum/shasum not found; skipping checksum verification" >&2
fi
if [ -n "$CHECK" ]; then
  download "$BASE_URL/SHA256SUMS" "$TMP_DIR/SHA256SUMS" || exit 1
  EXPECTED="$(grep -F "$ASSET" "$TMP_DIR/SHA256SUMS" | awk '{print $1}')"
  ACTUAL="$($CHECK "$ARCHIVE" | awk '{print $1}')"
  [ -n "$EXPECTED" ] && [ "$EXPECTED" = "$ACTUAL" ] || {
    echo "checksum verification failed for $ASSET" >&2
    exit 1
  }
fi

case "$ASSET" in
  *.tar.gz|*.tgz)
    need_cmd tar
    tar -xzf "$ARCHIVE" -C "$TMP_DIR"
    ;;
  *.zip)
    need_cmd unzip
    unzip -q "$ARCHIVE" -d "$TMP_DIR"
    ;;
  *) echo "unsupported archive format: $ASSET" >&2; exit 1 ;;
esac

BINARY_PATH="$(find "$TMP_DIR" -type f -name "$BIN_NAME" | head -n 1)"
[ -n "$BINARY_PATH" ] || {
  echo "binary $BIN_NAME not found in the downloaded archive" >&2
  exit 1
}

mkdir -p "$INSTALL_DIR"
install -m 0755 "$BINARY_PATH" "$INSTALL_DIR/$BIN_NAME"
echo "installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"
