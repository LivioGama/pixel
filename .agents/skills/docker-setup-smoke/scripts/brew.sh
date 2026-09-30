#!/bin/sh
set -eu
# Homebrew's prefix belongs to linuxbrew and is not readable by other users,
# so that user runs both the formula install and the checks, as on a Mac
# where the person who installed Homebrew runs pixel.
brew=/home/linuxbrew/.linuxbrew/bin/brew
chown linuxbrew:linuxbrew /evidence
su linuxbrew -c "$brew install LivioGama/tap/pixel" > /evidence/brew-install.log 2>&1 \
    || { cat /evidence/brew-install.log; exit 1; }
tail -3 /evidence/brew-install.log
su linuxbrew -c "$brew deps --installed LivioGama/tap/pixel" > /evidence/brew-deps.txt
su linuxbrew -c "$brew info --json=v2 LivioGama/tap/pixel" > /evidence/brew-info.json
dir=/home/linuxbrew/.linuxbrew/bin
"$dir/pixel" --version > /evidence/binary-version.txt
cat /evidence/binary-version.txt
printf 'PIXEL_BIN_DIR=%s\nPIXEL_OWNS_BINARY=0\n' "$dir" > /etc/pixel-smoke.env
