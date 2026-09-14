#!/bin/sh
# pixel smoke test — exercises the INSTALLED pixel end to end: CLI surface,
# the guard hook's advisory contract across agent tool names, session-start,
# doctor, the install surface, and the help of the mandatory workflows.
#
#   scripts/pixel-smoke-test.sh                 # binary from `command -v pixel`
#   PIXEL_BIN=target/dev-release/pixel scripts/pixel-smoke-test.sh
#   PIXEL_SHELL=fish scripts/pixel-smoke-test.sh # doctor --shell when the
#                                               # login shell is not the one
#                                               # `claude` is launched from
#
# Read-only: nothing under $HOME is written. Run `pixel install` first; the
# doctor section reports what it left non-green. Exit 1 on any failure.
#
# The guard hook never blocks (see crates/pixel/src/guard.rs): destructive or
# substitutable git commands get an ADVISORY (exit 0, JSON note with a pixel
# alternative), a grep/rg on one file gets a transparent REWRITE (exit 0,
# `updatedInput` pointing at `pixel search-compat`), and everything else
# passes through silently. Those three shapes are what this test asserts.
set -u

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PIXEL="${PIXEL_BIN:-$(command -v pixel 2>/dev/null || true)}"
if [ -z "$PIXEL" ]; then
    for p in "$ROOT/target/dev-release/pixel" "$ROOT/target/release/pixel"; do
        [ -x "$p" ] && PIXEL="$p" && break
    done
fi
if [ -z "$PIXEL" ] || [ ! -x "$PIXEL" ]; then
    echo "pixel-smoke-test: no pixel binary (PIXEL_BIN unset, none on PATH, none under target/)." >&2
    echo "  build + install one: pixel self-update --repo . --build \"cargo build --profile dev-release -p pixel-cli\"" >&2
    exit 2
fi
REPO="$ROOT"
DOCTOR_SHELL=""
[ -n "${PIXEL_SHELL:-}" ] && DOCTOR_SHELL="--shell $PIXEL_SHELL"

PASS=0; FAIL=0
ok() { echo "PASS: $1"; PASS=$((PASS+1)); }
no() { echo "FAIL: $1 — $2"; FAIL=$((FAIL+1)); }

# payload <tool_name> <input_key> <input_value> [event]
payload() {
    printf '{"hook_event_name":"%s","tool_name":"%s","cwd":"%s","tool_input":{"%s":"%s"}}' \
        "${4:-PreToolUse}" "$1" "$REPO" "$2" "$3"
}
# guard <payload> -> sets OUT and CODE
guard() {
    OUT=$(printf '%s' "$1" | "$PIXEL" hook guard 2>/dev/null); CODE=$?
}
# json_field <json> <python expression over d> -> prints the value or ""
json_field() {
    printf '%s' "$1" | python3 -c "
import json,sys
try:
    d=json.load(sys.stdin)
except Exception:
    print(''); sys.exit(0)
try:
    print($2)
except Exception:
    print('')"
}
expect_advisory() { # label needle
    ctx=$(json_field "$OUT" "d['hookSpecificOutput']['additionalContext']")
    if [ "$CODE" -eq 0 ] && printf '%s' "$ctx" | grep -q -- "$2"; then ok "$1: advisory names \`$2\`"
    else no "$1" "expected exit 0 + advisory containing \`$2\`, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
}
expect_rewrite() { # label
    cmd=$(json_field "$OUT" "d['hookSpecificOutput']['updatedInput']['command']")
    case "$cmd" in
        "pixel search-compat "*) [ "$CODE" -eq 0 ] && ok "$1: rewritten to \`pixel search-compat\`" || no "$1" "exit $CODE" ;;
        *) no "$1" "expected updatedInput.command = pixel search-compat …, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)" ;;
    esac
}
expect_silent() { # label
    if [ "$CODE" -eq 0 ] && [ -z "$OUT" ]; then ok "$1: passthrough (exit 0, no output)"
    else no "$1" "expected exit 0 + empty output, got exit $CODE: $(printf '%s' "$OUT" | head -c 200)"; fi
}
expect_proceeds() { # label — exit 0, and if anything was printed it is JSON
    if [ "$CODE" -ne 0 ]; then no "$1" "exit $CODE"; return; fi
    if [ -z "$OUT" ] || [ -n "$(json_field "$OUT" "'json'")" ]; then ok "$1: proceeds (exit 0)"
    else no "$1" "non-JSON output: $(printf '%s' "$OUT" | head -c 200)"; fi
}

SRC="$REPO/crates/pixel/src/main.rs"
RESET="git reset --hard HEAD~1"
GREP="grep -n login_user README.md"

echo "=== 0. Binary ==="
echo "  $PIXEL"
"$PIXEL" --version 2>/dev/null | sed 's/^/  /'

echo "=== 1. CLI surface ==="
"$PIXEL" --version 2>/dev/null | grep -q '^commit: ' && ok "--version reports commit/target/rustc/built" || no "--version" "no \`commit:\` line"
[ "$("$PIXEL" -V 2>/dev/null | wc -l | tr -d ' ')" = 1 ] && ok "-V is one line" || no "-V" "expected one line"
"$PIXEL" --help 2>&1 | grep -q "pixel" && ok "--help" || no "--help" "no output"

echo "=== 2. Guard hook — Claude tool names ==="
guard "$(payload Bash command "$RESET")";           expect_advisory "Claude Bash reset --hard" "pixel rescue"
guard "$(payload Bash command "git commit -m x")";  expect_advisory "Claude Bash git commit" "pixel publish"
guard "$(payload Bash command "$GREP")";            expect_rewrite  "Claude Bash grep on one file"
guard "$(payload Read file_path "$SRC")";           expect_proceeds "Claude Read"
guard "$(payload Edit file_path "$SRC")";           expect_proceeds "Claude Edit"

echo "=== 3. Guard hook — Devin tool names ==="
guard "$(payload exec command "$RESET")";           expect_advisory "Devin exec reset --hard" "pixel rescue"
guard "$(payload exec command "$GREP")";            expect_rewrite  "Devin exec grep on one file"
guard "$(payload read file_path "$SRC")";           expect_proceeds "Devin read"
guard "$(payload edit file_path "$SRC")";           expect_proceeds "Devin edit"
guard "$(payload find_file_by_name pattern "*.rs")"; expect_proceeds "Devin find_file_by_name"

echo "=== 3b. Guard hook — Codex tool names ==="
guard "$(payload bash command "$RESET")";           expect_advisory "Codex bash reset --hard" "pixel rescue"
guard "$(payload apply_patch file_path "$SRC")";    expect_proceeds "Codex apply_patch"
guard "$(payload glob pattern "*.rs")";             expect_proceeds "Codex glob"
OUT=$(payload shell command "$GREP" | "$PIXEL" hook guard --provider codex 2>/dev/null); CODE=$?
expect_rewrite "Codex --provider codex shell grep on one file"

echo "=== 3c. Guard hook — Gemini tool names ==="
guard "$(payload run_shell_command command "$RESET")"; expect_advisory "Gemini run_shell_command reset --hard" "pixel rescue"
guard "$(payload read_file file_path "$SRC")";      expect_proceeds "Gemini read_file"
guard "$(payload write_file file_path "$SRC")";     expect_proceeds "Gemini write_file"
guard "$(payload search pattern "test")";           expect_proceeds "Gemini search"

echo "=== 4. Guard hook — unknown tool name ==="
guard "$(payload webfetch url "https://example.invalid")"; expect_silent "unknown tool"

echo "=== 5. Guard hook — non-PreToolUse event ==="
guard "$(payload exec command "$RESET" PostToolUse)"; expect_silent "PostToolUse"

echo "=== 6. Guard hook — PIXEL_TARGETS_GUARD=0 override ==="
OUT=$(payload Bash command "$RESET" | PIXEL_TARGETS_GUARD=0 "$PIXEL" hook guard 2>/dev/null); CODE=$?
expect_silent "PIXEL_TARGETS_GUARD=0"

echo "=== 7. Session-start hook ==="
OUT=$(printf '{}' | "$PIXEL" hook session-start 2>/dev/null); CODE=$?
[ "$CODE" -eq 0 ] && printf '%s' "$OUT" | grep -q capabilities && ok "session-start emits the capability block" || no "session-start" "exit $CODE"

echo "=== 8. Doctor ==="
# shellcheck disable=SC2086
DOC=$("$PIXEL" doctor "$REPO" --json $DOCTOR_SHELL 2>/dev/null)
printf '%s' "$DOC" | python3 -c '
import json,sys
d=json.load(sys.stdin)
s=d["summary"]; print("  green:",s["green"],"yellow:",s["yellow"],"red:",s["red"])
for c in d["checks"]:
    if c["status"]!="green": print("  ", c["status"].upper(), c["id"], "—", c["summary"])
sys.exit(0 if d["ok"] else 1)' && ok "doctor: ok" || no "doctor" "not ok (non-green checks listed above; PIXEL_SHELL=<shell> if only install.shell-wrappers is red)"

echo "=== 9. Install surface (what \`pixel install\` deploys, read through doctor) ==="
for id in install.agent-prompt install.subagent-prompt install.shell-wrappers install.codex-config rule.parity rule.scenarios; do
    st=$(json_field "$DOC" "next(c['status'] for c in d['checks'] if c['id']=='$id')")
    [ "$st" = green ] && ok "doctor $id green" || no "doctor $id" "status '${st:-missing}'"
done
[ -s "$HOME/.local/share/pixel/agent-prompt.md" ] && ok "agent-prompt.md deployed" || no "agent-prompt.md" "missing at ~/.local/share/pixel (run: pixel install)"

echo "=== 10. Mandatory workflows + release gate — help surface ==="
for cmd in targets resolve rescue reconcile release-check upgrade; do
    "$PIXEL" "$cmd" --help 2>&1 | grep -q "$cmd" && ok "$cmd --help" || no "$cmd --help" "no output"
done
"$PIXEL" uninstall --help 2>&1 | grep -q -- "--wrappers-only" && ok "uninstall --wrappers-only documented" || no "uninstall --help" "no --wrappers-only"

echo ""
echo "=== RESULTS ==="
echo "PASS: $PASS  FAIL: $FAIL"
if [ "$FAIL" -eq 0 ]; then echo "ALL GREEN"; exit 0; else echo "HAS FAILURES"; exit 1; fi
