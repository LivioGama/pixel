#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --help ]]; then
    echo "Usage: bash $0 [vX.Y.Z] (default: v0.6.1)"
    exit 0
fi
release=${1:-v0.6.1}
if [[ $# -gt 1 || ! $release =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo 'Expected one release tag, e.g. v0.6.1' >&2
    exit 2
fi
scripts=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$scripts/../../../.." && pwd)
docker info >/dev/null
mkdir -p "$repo/target/docker-setup-smoke"
evidence=$(mktemp -d "$repo/target/docker-setup-smoke/run-XXXXXX")
container="pixel-setup-smoke-${evidence##*/}-$$"
image='debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251'
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "Evidence: $evidence"
{
    printf 'checkout: %s\nrelease: %s\nimage: %s\n' "$(git -C "$repo" rev-parse HEAD)" "$release" "$image"
    printf 'command: bash %q %q\n' "$0" "$release"
    docker version
} > "$evidence/identity.txt"
# Retain the stopped container just long enough to export failed checks too.
set +e
docker run --name "$container" \
    --mount "type=bind,src=$scripts,dst=/checks,readonly" \
    --env "PIXEL_RELEASE=$release" "$image" \
    sh -ec 'mkdir /evidence; timeout 300 sh /checks/bootstrap.sh; timeout 180 su - tester -s /bin/sh -c "sh /checks/checks.sh"' \
    > "$evidence/run.log" 2>&1
status=$?
set -e
if ! docker cp "$container:/evidence/." "$evidence/"; then
    echo 'Could not export container evidence' >&2
    if [[ $status -eq 0 ]]; then status=1; fi
fi
printf '%s\n' "$status" > "$evidence/exit-status.txt"
cat "$evidence/run.log"
exit "$status"
