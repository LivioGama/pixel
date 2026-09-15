#!/bin/sh
# pixel install script — downloads the latest release binary from GitHub.
# Usage: curl -fsSL https://raw.githubusercontent.com/LivioGama/pixel/main/scripts/install.sh | sh
set -eu

REPO="LivioGama/pixel"
INSTALL_DIR="${PIXEL_INSTALL_DIR:-${HOME}/.local/bin}"

# A SHA-256 digest exactly as the release workflow writes it: 64 lower-case
# hex digits. The digits are spelled out one by one because a `[0-9a-f]` range
# is collation-dependent: in a UTF-8 locale, macOS's bash matches `[!0-9a-f]`
# against an upper-case letter.
is_sha256() {
    case "$1" in
        "" | *[!0123456789abcdef]*) return 1 ;;
    esac
    [ "${#1}" -eq 64 ]
}

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

# Verify checksum. Each stage is checked on its own: the script runs under
# `set -eu` without pipefail, so a pipeline's status is its last command's and
# a failed `curl` in `curl ... | awk ...` left the digest empty — the
# installer then blamed the archive ("Checksum mismatch") for a network error.
# Only a download it could not make, a digest it could not read, or two valid
# digests that differ fail here.
echo "Verifying checksum..."

if ! CHECKSUM_FILE=$(curl -fsSL "$SHA_URL"); then
    echo "Could not download ${SHA_URL} (curl error above)." >&2
    echo "Refusing to install ${ARCHIVE} without its checksum." >&2
    exit 1
fi
EXPECTED=$(printf '%s\n' "$CHECKSUM_FILE" | awk '{print $1}')
if ! is_sha256 "$EXPECTED"; then
    echo "Could not read a SHA-256 digest from ${SHA_URL} (got '${EXPECTED}')." >&2
    exit 1
fi

# sha256sum is the coreutils tool busybox and Alpine images ship; shasum is a
# Perl script they lack, so on the musl target the first is the one present.
if command -v sha256sum >/dev/null 2>&1; then
    HASH_TOOL="sha256sum"
elif command -v shasum >/dev/null 2>&1; then
    HASH_TOOL="shasum -a 256"
else
    echo "Neither sha256sum nor shasum is installed; cannot verify ${ARCHIVE}." >&2
    echo "Install coreutils (sha256sum) or perl (shasum) and retry." >&2
    exit 1
fi
if ! DIGEST=$($HASH_TOOL "${TMPDIR}/${ARCHIVE}"); then
    echo "Could not compute the SHA-256 of ${ARCHIVE} with ${HASH_TOOL} (error above)." >&2
    exit 1
fi
ACTUAL=$(printf '%s\n' "$DIGEST" | awk '{print $1}')
if ! is_sha256 "$ACTUAL"; then
    echo "Could not read a SHA-256 digest for ${ARCHIVE} from ${HASH_TOOL} (got '${ACTUAL}')." >&2
    exit 1
fi

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
