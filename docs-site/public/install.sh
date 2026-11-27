#!/bin/sh
set -eu

REPO="signalridge/mare"
INSTALL_DIR="${MARE_INSTALL_DIR:-$HOME/.local/bin}"

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "mare install: required command not found: $1" >&2
    exit 1
  fi
}

need_cmd curl
need_cmd tar
need_cmd awk

OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
  Darwin) OS_PART="apple-darwin" ;;
  Linux)  OS_PART="unknown-linux-gnu" ;;
  *)
    echo "mare install: unsupported operating system: $OS" >&2
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)  ARCH_PART="x86_64" ;;
  arm64|aarch64) ARCH_PART="aarch64" ;;
  *)
    echo "mare install: unsupported CPU architecture: $ARCH" >&2
    exit 1
    ;;
esac

TARGET="${ARCH_PART}-${OS_PART}"

if [ -n "${MARE_VERSION:-}" ]; then
  case "$MARE_VERSION" in
    v*) TAG="$MARE_VERSION" ;;
    *)  TAG="v$MARE_VERSION" ;;
  esac
else
  LATEST_URL="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest")"
  TAG="${LATEST_URL##*/}"
  if [ -z "$TAG" ] || [ "$TAG" = "latest" ]; then
    echo "mare install: failed to resolve latest release tag from $LATEST_URL" >&2
    exit 1
  fi
fi

ARCHIVE="mare-${TAG}-${TARGET}.tar.gz"
BASE_URL="https://github.com/${REPO}/releases/download/${TAG}"

TMPDIR_MARE="$(mktemp -d)"
trap 'rm -rf "$TMPDIR_MARE"' EXIT

echo "Installing mare ${TAG} (${TARGET})..."
curl -fsSL "${BASE_URL}/${ARCHIVE}" -o "${TMPDIR_MARE}/${ARCHIVE}"
curl -fsSL "${BASE_URL}/checksums.txt" -o "${TMPDIR_MARE}/checksums.txt"

EXPECTED="$(awk -v f="$ARCHIVE" '$2 == f || $2 == ("*" f) { print tolower($1); exit }' "${TMPDIR_MARE}/checksums.txt")"
if [ -z "$EXPECTED" ]; then
  echo "mare install: missing checksum for ${ARCHIVE} in checksums.txt" >&2
  exit 1
fi

if command -v sha256sum >/dev/null 2>&1; then
  ACTUAL="$(sha256sum "${TMPDIR_MARE}/${ARCHIVE}" | awk 'NR==1 { print tolower($1) }')"
elif command -v shasum >/dev/null 2>&1; then
  ACTUAL="$(shasum -a 256 "${TMPDIR_MARE}/${ARCHIVE}" | awk 'NR==1 { print tolower($1) }')"
else
  echo "mare install: neither sha256sum nor shasum found" >&2
  exit 1
fi

if [ "$EXPECTED" != "$ACTUAL" ]; then
  echo "mare install: checksum mismatch for ${ARCHIVE}: expected=${EXPECTED}, actual=${ACTUAL}" >&2
  exit 1
fi

tar -xzf "${TMPDIR_MARE}/${ARCHIVE}" -C "${TMPDIR_MARE}"
mkdir -p "$INSTALL_DIR"
install -m 755 "${TMPDIR_MARE}/mare-${TAG}-${TARGET}/mare" "${INSTALL_DIR}/mare"

"${INSTALL_DIR}/mare" --version
case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *)
    echo ""
    echo "Installed mare to ${INSTALL_DIR}/mare"
    echo "Add it to your PATH, e.g.:"
    echo "  export PATH=\"${INSTALL_DIR}:\$PATH\""
    ;;
esac
