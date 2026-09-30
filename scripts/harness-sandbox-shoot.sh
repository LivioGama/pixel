#!/usr/bin/env bash
# harness-sandbox-shoot.sh — one command: sandbox up, latest pixel, four
# harnesses shooting the same natural prompt, four videos on the pull request.
#
#   scripts/harness-sandbox-shoot.sh [PR] [prompt]
#
# The sandbox is the OrbStack VM `pixel` (a real Linux box, no cargo: the
# released pixel comes from the linuxbrew tap). Antigravity is shot on the
# Mac — its login lives in the macOS Keychain, which the VM cannot read —
# against a local clone of the same repository. Both sides share one tmux
# server (OrbStack shares /tmp), so the live grid and the sessions interleave.
#
# The prompt is a plain build task with no tool hints and no mention of
# pixel: the point is measuring pixel discovery — whether each harness,
# with pixel installed, reaches for pixel on its own. Trust/consent dialogs
# are pre-accepted by config where a CLI has one (Claude's bypass warning,
# idempotent json edit); Antigravity's workspace-trust dialog has no config
# form, so its tmux start keys press Enter.
#
# Sandbox swap-in point: Rivet AgentOS (WASM isolates, agent packages for
# Claude/Codex/pi) would replace the VM backend, but the CLIs need real home
# configs and native binaries — OrbStack stays the default until a shoot
# runs inside AgentOS.

set -u

PR="${1:-}"
PROMPT="${2:-Create the \"story\" feature}"
VM="pixel"
SHOOT4="/Users/livio/Downloads/shoot4"   # shared via OrbStack's /tmp-like mounts
OUTDIR="$SHOOT4"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RECORDER="$SCRIPT_DIR/harness-recorder.sh"
REPO_ORIGIN="git@github.com:LivioGama/facebook-clone.git"
AGY_MAC_REPO="${AGY_MAC_REPO:-$HOME/Documents/facebook-clone-agy}"
GRID_MARKER_BEGIN="<!-- sandbox-grid:begin -->"
GRID_MARKER_END="<!-- sandbox-grid:end -->"

die() { echo "harness-sandbox-shoot: $*" >&2; exit 2; }
command -v orb >/dev/null 2>&1 || die "orb not found (OrbStack)"
command -v gh >/dev/null 2>&1 || die "gh not found"
command -v agg >/dev/null 2>&1 || die "agg not found (brew install agg)"
command -v tmux >/dev/null 2>&1 || die "tmux not found (brew install tmux)"

# ── 1. Sandbox up: VM running, recording tools in, pixel latest + install ──
orb start "$VM" 2>/dev/null || true
orb -m "$VM" bash -lc '
    command -v tmux >/dev/null 2>&1 || /home/linuxbrew/.linuxbrew/bin/brew install -q tmux
    command -v asciinema >/dev/null 2>&1 || {
        curl -sL -o ~/.local/bin/asciinema \
            https://github.com/asciinema/asciinema/releases/download/v3.2.1/asciinema-aarch64-unknown-linux-gnu
        chmod +x ~/.local/bin/asciinema
    }
    /home/linuxbrew/.linuxbrew/bin/brew upgrade pixel >/dev/null 2>&1 \
        || echo "  (keeping the installed pixel; the tap has no newer bottle yet)" >&2
    /home/linuxbrew/.linuxbrew/bin/pixel install >/dev/null 2>&1 || true
    python3 - << "PY"
import json, os
p = os.path.expanduser("~/.claude.json")
d = json.load(open(p)) if os.path.exists(p) and os.path.getsize(p) else {}
d["bypassPermissionsModeAccepted"] = True
d.setdefault("projects", {})
d["projects"]["/home/livio/facebook-clone-claude-code"] = d["projects"].get(
    "/home/livio/facebook-clone-claude-code", {})
d["projects"]["/home/livio/facebook-clone-claude-code"]["bypassPermissionsModeAccepted"] = True
json.dump(d, open(p, "w"))
PY
' || die "sandbox prep failed"
echo "harness-sandbox-shoot: sandbox ready (pixel $($VM-shell pixel --version 2>/dev/null || orb -m $VM bash -lc 'pixel --version'))"

# ── 2. Repos: one per harness, pre-indexed off camera ──────────────────────
orb -m "$VM" bash -lc 'for d in /home/livio/facebook-clone-claude-code /home/livio/facebook-clone-codex /home/livio/facebook-clone-devin; do
    /home/linuxbrew/.linuxbrew/bin/pixel build-index "$d" >/dev/null 2>&1
done' || true
if [ ! -d "$AGY_MAC_REPO/.git" ]; then
    git clone -q "$REPO_ORIGIN" "$AGY_MAC_REPO" || die "cannot clone the sandbox repo for agy"
fi
pixel build-index "$AGY_MAC_REPO" >/dev/null 2>&1 || true

# ── 3. Shoot: four recorders in parallel ───────────────────────────────────
mkdir -p "$OUTDIR"
for spec in "claude|/home/livio/facebook-clone-claude-code|vm" \
            "codex|/home/livio/facebook-clone-codex|vm" \
            "pi|/home/livio/facebook-clone-devin|vm" \
            "agy|$AGY_MAC_REPO|mac"; do
    provider="${spec%%|*}"
    rest="${spec#*|}"
    repo="${rest%%|*}"
    host="${rest##*|}"
    if [ "$host" = "vm" ]; then
        orb -m "$VM" bash -lc "cd /Users/livio/Downloads && env IS_SANDBOX=1 \
            HARNESS_PROMPT_FULL=\"$PROMPT\" HARNESS_OUTDIR=/Users/livio/Downloads/shoot4 \
            HARNESS_INTERACTIVE_MAX=300 HARNESS_INTERACTIVE_TMUX=1 \
            PATH=/home/linuxbrew/.linuxbrew/bin:/home/livio/.local/bin:\$PATH \
            bash /Users/livio/Downloads/harness-recorder.sh \
            --provider $provider --repo $repo --scenario rns --interactive" \
            > "$OUTDIR/shoot-$provider.log" 2>&1 &
    else
        HARNESS_PROMPT_FULL="$PROMPT" HARNESS_OUTDIR="$OUTDIR" \
            HARNESS_INTERACTIVE_MAX=300 HARNESS_INTERACTIVE_IDLE=90 \
            HARNESS_INTERACTIVE_TMUX=1 \
            bash "$RECORDER" \
            --provider "$provider" --repo "$repo" --scenario rns --interactive \
            > "$OUTDIR/shoot-$provider.log" 2>&1 &
    fi
done
echo "harness-sandbox-shoot: four shoots running (claude, codex, pi in the VM; agy on the Mac)"
wait
echo "harness-sandbox-shoot: all shoots landed"

# ── 4. Videos: cast seconds → agg speed, ≤15 s each ────────────────────────
for provider in claude codex pi agy; do
    cast="$OUTDIR/harness-$provider-rns.cast"
    [ -s "$cast" ] || { echo "  missing cast: $provider" >&2; continue; }
    secs=$(python3 -c "
import json, sys
last = 0.0
for line in open('$cast'):
    try: e = json.loads(line)
    except json.JSONDecodeError: continue
    if isinstance(e, list) and len(e) > 2 and e[1] == 'o' and isinstance(e[0], (int, float)):
        last = max(last, e[0])
print(max(1, round(last)))")
    speed=$(( (secs + 14) / 15 )); [ "$speed" -lt 1 ] && speed=1
    agg "$cast" "$OUTDIR/harness-$provider-rns.gif" \
        --speed "$speed" --font-size 14 --theme asciinema \
        --cols 112 --rows 36 || true
    echo "  $provider: ${secs}s cast → gif at speed $speed"
done

# ── 5. Live 2x2 grid tmux (detached; watch with `tmux attach -t shoot-grid`)
/opt/homebrew/bin/tmux kill-session -t shoot-grid 2>/dev/null || true
/opt/homebrew/bin/tmux new-session -d -s shoot-grid -n grid \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-claude-code && exec claude --dangerously-skip-permissions \"$PROMPT\"'"
/opt/homebrew/bin/tmux split-window -h -t shoot-grid \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-codex && exec codex'"
/opt/homebrew/bin/tmux split-window -v -t shoot-grid.0 \
    "orb -m $VM bash -lc 'cd /home/livio/facebook-clone-devin && exec pi \"$PROMPT\"'"
/opt/homebrew/bin/tmux split-window -v -t shoot-grid.1 \
    "cd '$AGY_MAC_REPO' && exec bash -c '\"$HOME/.local/bin/agy\" -i=\"$PROMPT\" --dangerously-skip-permissions'"
/opt/homebrew/bin/tmux select-layout -t shoot-grid tiled
echo "  live grid ready: tmux attach -t shoot-grid"

# ── 6. Publish: GIFs to the media branch, counts into the PR description ──
if [ -n "$PR" ]; then
    (cd "$SCRIPT_DIR/.." && git fetch -q origin harness-recordings-media 2>/dev/null) || true
    base=$(git -C "$SCRIPT_DIR/.." rev-parse -q --verify FETCH_HEAD || true)
    if [ -z "$base" ]; then
        git -C "$SCRIPT_DIR/.." push -q origin "HEAD:refs/heads/harness-recordings-media"
        git -C "$SCRIPT_DIR/.." fetch -q origin harness-recordings-media
        base=$(git -C "$SCRIPT_DIR/.." rev-parse -q --verify FETCH_HEAD)
    fi
    export GIT_INDEX_FILE="$OUTDIR/media-index"
    git -C "$SCRIPT_DIR/.." read-tree "$(git -C "$SCRIPT_DIR/.." rev-parse "$base^{tree}")"
    for provider in claude codex pi agy; do
        gif="$OUTDIR/harness-$provider-rns.gif"
        [ -s "$gif" ] || continue
        blob=$(git -C "$SCRIPT_DIR/.." hash-object -w "$gif")
        git -C "$SCRIPT_DIR/.." update-index --add --cacheinfo 100644,$blob,recordings/grid/$provider-rns.gif
    done
    tree=$(git -C "$SCRIPT_DIR/.." write-tree)
    unset GIT_INDEX_FILE
    commit=$(git -C "$SCRIPT_DIR/.." -c user.email=pixel-recorder@local -c user.name=pixel-recorder \
        commit-tree "$tree" -p "$base" -m "sandbox grid: story-feature shoots for PR #$PR")
    git -C "$SCRIPT_DIR/.." push -q origin "$commit:refs/heads/harness-recordings-media" \
        || die "media push failed"
    echo "  media branch updated"
    gh pr edit "$PR" --body-file "$OUTDIR/pr-body.md" 2>/dev/null \
        || echo "  (write $OUTDIR/pr-body.md with the grid and rerun the edit)"
fi
echo "harness-sandbox-shoot: done — metas in $OUTDIR, grid on the media branch"
