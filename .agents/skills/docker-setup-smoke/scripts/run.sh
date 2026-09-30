#!/usr/bin/env bash
set -euo pipefail
if [[ ${1:-} == --help ]]; then
    echo "Usage: bash $0 [vX.Y.Z | --source main | --source SHA | --pr NUMBER]"
    echo 'Default: v0.6.1. Source builds use the fetched commit, not the PR merge ref.'
    exit 0
fi
mode=release
release=v0.6.1
source_ref=''
if [[ $# -eq 2 && $1 == --source && $2 == main ]]; then
    mode=source
    source_ref=refs/heads/main
elif [[ $# -eq 2 && $1 == --source && $2 =~ ^[a-fA-F0-9]{40}$ ]]; then
    mode=source
    source_ref=$2
elif [[ $# -eq 2 && $1 == --pr && $2 =~ ^[1-9][0-9]*$ ]]; then
    mode=source
    source_ref="refs/pull/$2/head"
elif [[ $# -eq 1 && $1 =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    release=$1
elif [[ $# -ne 0 ]]; then
    echo 'Expected a release tag, --source main/SHA, or --pr NUMBER' >&2
    exit 2
fi
scripts=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$scripts/../../../.." && pwd)
docker info >/dev/null
mkdir -p "$repo/target/docker-setup-smoke"
evidence=$(mktemp -d "$repo/target/docker-setup-smoke/run-XXXXXX")
container="pixel-setup-smoke-${evidence##*/}-$$"
image='debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251'
bootstrap=bootstrap.sh
budget=300
if [[ $mode == source ]]; then
    release=''
    image='rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e'
    bootstrap=source.sh
    budget=1800
fi
trap 'docker rm -f "$container" >/dev/null 2>&1 || true' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
echo "Evidence: $evidence"
{
    printf 'checkout: %s\nmode: %s\nrelease: %s\nsource-ref: %s\nimage: %s\n' \
        "$(git -C "$repo" rev-parse HEAD)" "$mode" "$release" "$source_ref" "$image"
    printf 'command: bash %q' "$0"
    if [[ $# -gt 0 ]]; then printf ' %q' "$@"; fi
    printf '\n'
    docker version
} > "$evidence/identity.txt"
# Retain the stopped container just long enough to export failed checks too.
set +e
docker run --name "$container" \
    --cpus 4 --memory 6g \
    --mount "type=bind,src=$scripts,dst=/checks,readonly" \
    --env "PIXEL_RELEASE=$release" --env "PIXEL_SOURCE_REF=$source_ref" \
    --env "PIXEL_BOOTSTRAP=$bootstrap" --env "PIXEL_BOOTSTRAP_TIMEOUT=$budget" "$image" \
    sh -ec 'mkdir /evidence; timeout "$PIXEL_BOOTSTRAP_TIMEOUT" sh "/checks/$PIXEL_BOOTSTRAP"; timeout 180 su - tester -s /bin/sh -c "sh /checks/checks.sh"' \
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
