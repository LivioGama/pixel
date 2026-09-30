#!/bin/sh
set -eu
case "$(uname -m)" in
    aarch64) target=aarch64-unknown-linux-musl ;;
    x86_64) target=x86_64-unknown-linux-musl ;;
    *) echo 'Unsupported Linux architecture' >&2; exit 2 ;;
esac
archive="pixel-${PIXEL_RELEASE}-${target}.tar.gz"
url="https://github.com/LivioGama/pixel/releases/download/${PIXEL_RELEASE}/${archive}"
cd /tmp
curl --fail --silent --show-error --location --max-time 120 "$url" -o "$archive"
curl --fail --silent --show-error --location --max-time 120 "$url.sha256" -o "$archive.sha256"
sha256sum --check "$archive.sha256"
mkdir unpack
tar -xzf "$archive" -C unpack
install -o tester -g tester -m 755 "unpack/pixel-${PIXEL_RELEASE}-${target}/bin/pixel" /home/tester/.local/bin/pixel
/home/tester/.local/bin/pixel --version
test "$(/home/tester/.local/bin/pixel -V)" = "pixel ${PIXEL_RELEASE#v}"
