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

# Resolve the latest release tag from the redirect of the releases/latest page,
# not from api.github.com: the anonymous REST API allows 60 requests an hour per
# IP, which a shared address (CI runners, an office NAT) exhausts. The page
# redirects to .../releases/tag/<tag>, or to .../releases when there is none.
echo "Fetching latest release..."
if ! LATEST_URL=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/${REPO}/releases/latest"); then
    echo "Could not reach https://github.com/${REPO}/releases/latest (curl error above)." >&2
    exit 1
fi
case "$LATEST_URL" in
    */releases/tag/v?*) VERSION="${LATEST_URL##*/releases/tag/}" ;;
    *)
        echo "No prebuilt release found for ${REPO}." >&2
        echo "Install from source instead:" >&2
        echo "  cargo install --git https://github.com/${REPO} --force" >&2
        exit 1
        ;;
esac
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

# Install — atomic rename to avoid corrupting a running binary's code
# signature on macOS (in-place cp overwrites a mapped Mach-O, invalidating
# the ad-hoc signature and causing SIGKILL on next invocation).
mkdir -p "$INSTALL_DIR"
BINARY="${TMPDIR}/pixel-${VERSION}-${TARGET}/bin/pixel"
if [ ! -f "$BINARY" ]; then
    # Fallback: some archives may not have the version-prefixed dir
    BINARY="${TMPDIR}/bin/pixel"
fi
DEST="${INSTALL_DIR}/pixel"
TMP_DEST="${INSTALL_DIR}/.pixel.tmp.$$"
cp "$BINARY" "$TMP_DEST"
chmod +x "$TMP_DEST"
mv -f "$TMP_DEST" "$DEST"

echo "Installed pixel to ${INSTALL_DIR}/pixel"
echo "Add ${INSTALL_DIR} to your PATH if it's not already there."
echo "Run: pixel doctor"
