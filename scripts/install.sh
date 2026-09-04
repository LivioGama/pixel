#!/bin/sh
# pixel install script — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/LivioGama/pixel/main/scripts/install.sh | sh
set -eu

REPO="LivioGama/pixel"
INSTALL_DIR="${PIXEL_INSTALL_DIR:-${HOME}/.local/bin}"

# Detect OS + arch
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Darwin) OS_TARGET="apple-darwin" ;;
    Linux)  OS_TARGET="unknown-linux-musl" ;;
    *) echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
    x86_64|amd64) ARCH_TARGET="x86_64" ;;
    arm64|aarch64) ARCH_TARGET="aarch64" ;;
    *) echo "Unsupported arch: $ARCH" >&2; exit 1 ;;
esac

TARGET="${ARCH_TARGET}-${OS_TARGET}"

# Fetch latest release tag
echo "Fetching latest release..."
LATEST=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | head -1 | sed -E 's/.*"v?([^"]+)".*/\1/')
if [ -z "$LATEST" ]; then
    echo "Could not determine latest version." >&2
    exit 1
fi
VERSION="v${LATEST}"
ARCHIVE="pixel-${VERSION}-${TARGET}.tar.gz"
URL="https://github.com/${REPO}/releases/download/${VERSION}/${ARCHIVE}"
SHA_URL="${URL}.sha256"

echo "pixel ${VERSION} (${TARGET})"

# Download
TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading ${ARCHIVE}..."
curl -fsSL "$URL" -o "${TMPDIR}/${ARCHIVE}"

# Verify checksum
echo "Verifying checksum..."
EXPECTED=$(curl -fsSL "$SHA_URL" | awk '{print $1}')
ACTUAL=$(shasum -a 256 "${TMPDIR}/${ARCHIVE}" | awk '{print $1}')
if [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Checksum mismatch!" >&2
    echo "  expected: $EXPECTED" >&2
    echo "  actual:   $ACTUAL" >&2
    exit 1
fi
echo "Checksum OK."

# Extract
tar xzf "${TMPDIR}/${ARCHIVE}" -C "$TMPDIR"

# Install
mkdir -p "$INSTALL_DIR"
BINARY="${TMPDIR}/pixel-${VERSION}-${TARGET}/bin/pixel"
if [ ! -f "$BINARY" ]; then
    # Fallback: some archives may not have the version-prefixed dir
    BINARY="${TMPDIR}/bin/pixel"
fi
cp "$BINARY" "${INSTALL_DIR}/pixel"
chmod +x "${INSTALL_DIR}/pixel"

echo "Installed pixel to ${INSTALL_DIR}/pixel"
echo "Add ${INSTALL_DIR} to your PATH if it's not already there."
echo "Run: pixel doctor"
