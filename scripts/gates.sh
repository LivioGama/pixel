#!/bin/sh
# Local gate runner: the CI gates (fmt, clippy, test) with laptop-safe defaults.
#
#   scripts/gates.sh            # skip when nothing Rust-affecting changed
#   scripts/gates.sh --force    # run even when nothing changed
#   scripts/gates.sh --mutants  # also run cargo-mutants on the diff against main
#
# Why a script instead of three commands:
# - Skip-if-untouched: outside CI, when neither the diff against `main` nor
#   the working tree touches a Rust-affecting path (*.rs, Cargo.*, build.rs,
#   .cargo/, rust-toolchain*, rustfmt.toml, clippy.toml), the CARGO gates
#   cannot change outcome, so the script stops without compiling anything. A
#   docs-only turn costs seconds, not a workspace build. The release prepare
#   contract is not a cargo gate and runs either way: it compiles nothing, and
#   a change to prepare.sh or to its own test must not need --force to be
#   checked.
# - Laptop safety: every cargo invocation runs under `nice` and with
#   CARGO_BUILD_JOBS defaulting to ncpu-2 (two cores stay free, peak rustc
#   memory drops with the job count) and RUST_TEST_THREADS to ncpu/2 (the
#   integration tests each spawn a pixel binary plus git; eight at once is
#   what pushed a 16 GB machine into swap). An explicit value in the
#   environment always wins.
# - Fail open: when git cannot answer (not a repo, no `main`), the gates run.
#
# CI=1 (set by GitHub Actions) disables the skip and the nice/jobs defaults so
# the workflow keeps running exactly the documented commands.
set -eu

FORCE=0
MUTANTS=0
for arg in "$@"; do
    case "$arg" in
        --force) FORCE=1 ;;
        --mutants) MUTANTS=1 ;;
        -h|--help) sed -n '2,26p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *) echo "gates.sh: unknown argument: $arg" >&2; exit 2 ;;
    esac
done

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"

# Paths whose change can alter a gate's outcome. Anything else (docs, prompts,
# scripts, workflows) cannot make fmt/clippy/test go red.
rust_affecting() {
    grep -E -q '(^|/)(Cargo\.toml|Cargo\.lock|build\.rs|rust-toolchain(\.toml)?|rustfmt\.toml|clippy\.toml)$|\.rs$|(^|/)\.cargo/'
}

# Prints "run" when a gate could change outcome, "skip" when none can. Any
# git failure prints "run" (fail open).
gate_decision() {
    base="$(git merge-base main HEAD 2>/dev/null)" \
        || base="$(git merge-base origin/main HEAD 2>/dev/null)" \
        || { echo run; return; }
    committed="$(git diff --name-only "$base" HEAD 2>/dev/null)" || { echo run; return; }
    dirty="$(git status --porcelain --untracked-files=all 2>/dev/null | cut -c4-)" || { echo run; return; }
    if printf '%s\n%s\n' "$committed" "$dirty" | rust_affecting; then echo run; else echo skip; fi
}

if [ -z "${CI:-}" ]; then
    ncpu="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    # Negative CARGO_BUILD_JOBS means "ncpu + value" (cargo ≥ 1.66).
    export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:--2}"
    half=$((ncpu / 2)); [ "$half" -lt 2 ] && half=2
    export RUST_TEST_THREADS="${RUST_TEST_THREADS:-$half}"
    NICE="nice -n ${GATES_NICE:-10}"
else
    NICE=""
fi

step() {
    name="$1"; shift
    printf '\n==> %s\n' "$name"
    start=$(date +%s)
    if $NICE "$@"; then
        printf '<== %s ok (%ss)\n' "$name" "$(( $(date +%s) - start ))"
    else
        code=$?
        printf '<== %s FAILED (exit %s, %ss)\n' "$name" "$code" "$(( $(date +%s) - start ))" >&2
        exit "$code"
    fi
}

# The gate the CI job "Release prepare contract" runs. It stubs gh and cargo in
# a disposable repository and also runs `prepare.sh --check` against this tree,
# so it is the step that catches a fragment named for a section that does not
# exist, an entry left under ## [Unreleased], and a --check that refuses a tree
# release preparation produces. It compiles nothing (~6 s), which is why it
# runs before the cargo skip rather than under it: 0.4.0's release pull request
# went red on a gate no local run could reach.
step "release prepare contract" python3 scripts/test-prepare.py

if [ "$FORCE" -eq 0 ] && [ -z "${CI:-}" ] && [ "$(gate_decision)" = skip ]; then
    echo
    echo "gates.sh: no Rust-affecting change against main or in the working tree; skipping the cargo gates (use --force to run them)."
    exit 0
fi

step "cargo fmt --check" cargo fmt --all -- --check
step "cargo clippy" cargo clippy --workspace --all-targets -- -D warnings
# nextest (what CI runs, .config/nextest.toml) when installed, else cargo
# test. --no-fail-fast: a red test binary must not hide the ones after it.
if cargo nextest --version >/dev/null 2>&1; then
    step "cargo nextest" cargo nextest run --workspace --locked --no-fail-fast
    step "cargo test --doc" cargo test --workspace --locked --doc
else
    step "cargo test" cargo test --workspace --locked --no-fail-fast
fi

if [ "$MUTANTS" -eq 1 ]; then
    diff_file="$(mktemp)"
    git diff main...HEAD > "$diff_file"
    step "cargo mutants (in diff)" cargo mutants --in-diff "$diff_file"
    rm -f "$diff_file"
fi

echo
echo "gates.sh: all gates passed"
