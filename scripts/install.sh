#!/bin/sh

set -eu

REPO="${ACP_AGENT_REPO:-observerw/acp-agent-rs}"
BIN_NAME="${ACP_AGENT_BIN_NAME:-acp-agent}"
INSTALL_DIR="${ACP_AGENT_INSTALL_DIR:-$HOME/.local/bin}"
API_URL="https://api.github.com/repos/$REPO/releases/latest"

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

need_cmd curl
need_cmd install
need_cmd mktemp
need_cmd uname

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS="darwin" ;;
  Linux) OS="linux" ;;
  *)
    echo "unsupported operating system: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64|amd64) ARCH="x86_64" ;;
  *)
    echo "unsupported architecture: $ARCH" >&2
    exit 1
    ;;
esac

TMP_DIR="$(mktemp -d)"
cleanup() {
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT INT TERM

RELEASE_JSON="$TMP_DIR/release.json"
if [ -n "${GITHUB_TOKEN:-}" ]; then
  CURL_AUTH_HEADER="Authorization: Bearer $GITHUB_TOKEN"
  CURL_OK=0
  if curl -fsSL \
    -H "Accept: application/vnd.github+json" \
    -H "User-Agent: ${BIN_NAME}-install" \
    -H "$CURL_AUTH_HEADER" \
    "$API_URL" \
    -o "$RELEASE_JSON"; then
    CURL_OK=1
  fi
else
  CURL_OK=0
  if curl -fsSL \
    -H "Accept: application/vnd.github+json" \
    -H "User-Agent: ${BIN_NAME}-install" \
    "$API_URL" \
    -o "$RELEASE_JSON"; then
    CURL_OK=1
  fi
fi

if [ "$CURL_OK" -ne 1 ]; then
  echo "failed to fetch the latest GitHub release for $REPO" >&2
  echo "ensure the repository has a published release; set GITHUB_TOKEN if GitHub API access is rate-limited" >&2
  exit 1
fi

DOWNLOAD_URL="$(
  grep '"browser_download_url":' "$RELEASE_JSON" \
    | sed -E 's/.*"([^"]+)".*/\1/' \
    | grep "/${OS}[-_].*${ARCH}\\|/${ARCH}[-_].*${OS}" \
    | grep -E '\.(tar\.gz|tgz|zip)$' \
    | head -n 1
)"

if [ -z "$DOWNLOAD_URL" ]; then
  echo "no release asset found for ${OS}-${ARCH} in $REPO" >&2
  exit 1
fi

ARCHIVE_PATH="$TMP_DIR/asset"
curl -fsSL "$DOWNLOAD_URL" -o "$ARCHIVE_PATH"

EXTRACT_DIR="$TMP_DIR/extract"
mkdir -p "$EXTRACT_DIR"

case "$DOWNLOAD_URL" in
  *.zip)
    need_cmd unzip
    unzip -q "$ARCHIVE_PATH" -d "$EXTRACT_DIR"
    ;;
  *.tar.gz|*.tgz)
    need_cmd tar
    tar -xzf "$ARCHIVE_PATH" -C "$EXTRACT_DIR"
    ;;
  *)
    echo "unsupported archive format: $DOWNLOAD_URL" >&2
    exit 1
    ;;
esac

BINARY_PATH="$(
  find "$EXTRACT_DIR" -type f -name "$BIN_NAME" | head -n 1
)"

if [ -z "$BINARY_PATH" ]; then
  echo "binary $BIN_NAME not found in downloaded archive" >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
install -m 0755 "$BINARY_PATH" "$INSTALL_DIR/$BIN_NAME"

echo "installed $BIN_NAME to $INSTALL_DIR/$BIN_NAME"
