#!/bin/sh
set -eu
apt-get update -qq
apt-get install -y -qq curl ca-certificates git python3 >/dev/null
useradd -m -s /bin/bash tester
chown tester:tester /evidence
# Expand HOME in the login user's shell, not the root bootstrap shell.
# shellcheck disable=SC2016
su - tester -c 'mkdir -p "$HOME/.local/bin"'
