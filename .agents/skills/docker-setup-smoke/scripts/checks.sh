#!/bin/sh
set -eu
export PATH="$HOME/.local/bin:$PATH"
export PIXEL_METRICS=0
mkdir "$HOME/project"
cd "$HOME/project"
git init -q
printf '# user instruction\n' > AGENTS.md
cp AGENTS.md /evidence/original-AGENTS.md
report_ok() {
    python3 - "$1" <<'PY'
import json
import sys
with open(sys.argv[1]) as report:
    data = json.load(report)
assert data["ok"] is True, data
PY
}
pixel install --shell bash --json > /evidence/install-1.json
report_ok /evidence/install-1.json
test -s "$HOME/.local/share/pixel/agent-prompt.md"
cp "$HOME/.local/share/pixel/agent-prompt.md" /evidence/first-prompt.md
pixel install --shell bash --json > /evidence/install-2.json
report_ok /evidence/install-2.json
cmp /evidence/first-prompt.md "$HOME/.local/share/pixel/agent-prompt.md"
echo 'PASS global install twice, deployed prompt unchanged'
# Read the persisted value without the runner's metrics override.
env -u PIXEL_METRICS pixel config metrics off --global
env -u PIXEL_METRICS pixel config metrics > /evidence/metrics.txt
grep -q '^metrics: off' /evidence/metrics.txt
test -s "$HOME/.pixel/config.yaml"
echo 'PASS global config persistence'
pixel install --repo . --json > /evidence/repo-install.json
report_ok /evidence/repo-install.json
grep -q '^# user instruction$' AGENTS.md
echo 'PASS project install preserves user instructions'
pixel config classify off
if pixel classify test --label yes --label no > /evidence/classify.out 2>/evidence/classify.err; then
    echo 'FAIL disabled classify succeeded' >&2
    exit 1
fi
test ! -s /evidence/classify.out
grep -q 'classify is disabled' /evidence/classify.err
echo 'PASS disabled classify refuses without a model'
pixel uninstall --repo . --json > /evidence/repo-uninstall.json
report_ok /evidence/repo-uninstall.json
cmp /evidence/original-AGENTS.md AGENTS.md
echo 'PASS project uninstall restores user instructions exactly'
# Uninstall deletes the installed executable; a copy runs the second check.
cp "$HOME/.local/bin/pixel" /tmp/pixel-runner
pixel uninstall --shell bash --json > /evidence/uninstall-1.json
report_ok /evidence/uninstall-1.json
test ! -e "$HOME/.local/bin/pixel"
test ! -e "$HOME/.local/share/pixel/agent-prompt.md"
/tmp/pixel-runner uninstall --shell bash --binary-path "$HOME/.local/bin/pixel" --json > /evidence/uninstall-2.json
report_ok /evidence/uninstall-2.json
echo 'PASS global uninstall removes binary and prompt; repeat succeeds'
