#!/bin/sh
# Contract for the fast mutation-exposure preflight, using a disposable repo
# and a fake cargo so no mutation campaign or build runs.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
tmp=$(mktemp -d "${TMPDIR:-/tmp}/pixel-mutants-preflight-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/crates/demo/src" "$tmp/bin"
cp "$repo/scripts/mutants-preflight.sh" "$fixture/scripts/"

cat > "$tmp/bin/cargo" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$CARGO_LOG"
if [ "${CARGO_FAIL:-0}" = 1 ]; then
    echo "fake cargo failure" >&2
    exit 7
fi
printf '%b' "${CARGO_LISTING:-}"
EOF
chmod +x "$tmp/bin/cargo"

git -C "$fixture" init -q
git -C "$fixture" config user.email test@example.com
git -C "$fixture" config user.name test
printf 'fn base() {}\n' > "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add .
git -C "$fixture" commit -qm base
git -C "$fixture" branch -M main
git -C "$fixture" update-ref refs/remotes/origin/main HEAD
printf 'fn changed() {}\n' >> "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm rust-change

run() {
    PATH="$tmp/bin:$PATH" CARGO_LOG="$tmp/cargo.log" CARGO_LISTING='crates/demo/src/lib.rs:2:1: replace changed -> ()\n' \
        sh "$fixture/scripts/mutants-preflight.sh" "$@"
}

if (cd "$fixture" && run --check > "$tmp/blocked.out" 2>&1); then
    echo "expected an unacknowledged Rust change to block" >&2
    exit 1
fi
grep -q 'scripts/mutants-preflight.sh --ack' "$tmp/blocked.out"
grep -q '^mutants --list --in-diff ' "$tmp/cargo.log"

(cd "$fixture" && run --ack > "$tmp/ack.out")
(cd "$fixture" && run --check > "$tmp/allowed.out")
grep -q 'reviewed receipt matches' "$tmp/allowed.out"

printf 'fn another() {}\n' >> "$fixture/crates/demo/src/lib.rs"
git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm another-rust-change
if (cd "$fixture" && run --check > "$tmp/stale.out" 2>&1); then
    echo "expected a receipt for an older commit to block" >&2
    exit 1
fi
grep -q 'push blocked' "$tmp/stale.out"

git -C "$fixture" checkout -q -b docs-only refs/remotes/origin/main
printf 'docs\n' > "$fixture/README.md"
git -C "$fixture" add README.md
git -C "$fixture" commit -qm docs
: > "$tmp/cargo.log"
(cd "$fixture" && run --check > "$tmp/docs.out")
grep -q 'not applicable' "$tmp/docs.out"
test ! -s "$tmp/cargo.log"

git -C "$fixture" checkout -q -b dirty-rust refs/remotes/origin/main
printf 'fn unstaged() {}\n' >> "$fixture/crates/demo/src/lib.rs"
: > "$tmp/cargo.log"
if (cd "$fixture" && run --check > "$tmp/dirty.out" 2>&1); then
    echo "expected tracked, uncommitted Rust to block before listing" >&2
    exit 1
fi
grep -q 'commit or stash' "$tmp/dirty.out"
test ! -s "$tmp/cargo.log"

git -C "$fixture" add crates/demo/src/lib.rs
git -C "$fixture" commit -qm dirty-rust-change
if (cd "$fixture" && CARGO_FAIL=1 run --check > "$tmp/cargo-failure.out" 2>&1); then
    echo "expected a failed cargo-mutants listing to block" >&2
    exit 1
fi
grep -q 'cargo mutants --list failed' "$tmp/cargo-failure.out"

echo "mutants preflight contract: ok"
