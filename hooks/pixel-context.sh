#!/bin/sh
# pixel-context.sh — plugin lifecycle hook. Emits the pixel retrieval
# protocol as hookSpecificOutput.additionalContext so plugin installs get
# always-on instructions without `pixel install` or shell wrappers.
#
# Usage: pixel-context.sh <HookEventName>
# Registered for SessionStart and SubagentStart only — context persists for
# the session; per-prompt re-injection would waste tokens every turn.
#
# Non-blocking by contract: reads stdin to EOF (some harnesses require it),
# prints one JSON line, always exits 0.

EVENT="${1:-SessionStart}"
DIR=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
FILE="$DIR/PIXEL.md"

cat >/dev/null 2>&1 || :   # drain stdin; never block the harness
[ -f "$FILE" ] || exit 0   # missing context file → stay silent

python3 - "$EVENT" "$FILE" <<'PY'
import json, sys
event, path = sys.argv[1], sys.argv[2]
print(json.dumps({
    "hookSpecificOutput": {
        "hookEventName": event,
        "additionalContext": open(path, encoding="utf-8").read(),
    }
}))
PY
exit 0
