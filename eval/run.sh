#!/usr/bin/env bash
# eval/run.sh — repeatable harness trial: scenarios × arms × CLIs.
#
# Arms:
#   baseline   no pixel hooks, no AGENTS.md block, no pixel skill
#   on         one clean pixel hook set + committed AGENTS.md block + deployed payload
#   v<name>    one clean pixel hook set + payload from eval/variants/<name>/agent-prompt.md
#              + AGENTS.md block body from eval/variants/<name>/rules-body.md
#
# Env:
#   CLIS=claude            (claude|agy|codex|pi — codex/pi are stubs, see README)
#   ARMS="baseline on"     SCENARIOS="s1-hook-install ..."   MAX_TURNS=12
#   PIXEL_BIN=~/.local/bin/pixel
set -euo pipefail
EVAL_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO="$(git -C "$EVAL_DIR" rev-parse --show-toplevel)"
PIXEL_BIN="${PIXEL_BIN:-$HOME/.local/bin/pixel}"
DEPLOY_PROMPT="$HOME/.local/share/pixel/agent-prompt.md"
RESULTS="$EVAL_DIR/results"
SCRATCH="${SCRATCH:-/tmp/pixel-eval}"
EVAL_HEAD="$(git -C "$EVAL_DIR" rev-parse HEAD)"
MAIN_ROOT="$(cd "$(git -C "$EVAL_DIR" rev-parse --git-common-dir)/.." && pwd)"
CLIS="${CLIS:-claude}"
ARMS="${ARMS:-baseline on}"
SCENARIOS="${SCENARIOS:-s1-hook-install s2-vector-recall s3-rename-impact}"
MAX_TURNS="${MAX_TURNS:-12}"
mkdir -p "$RESULTS"

# Reused transcripts are only valid for the inputs that produced them; a
# mismatched results dir silently scores stale answers as current, which the
# gate cannot see. Refuse and let the operator archive instead.
RUN_IDENTITY="head=$EVAL_HEAD arms=$ARMS clis=$CLIS scenarios=$SCENARIOS turns=$MAX_TURNS"
IDENTITY_FILE="$RESULTS/.identity"
if [ -e "$IDENTITY_FILE" ] && [ "$(cat "$IDENTITY_FILE")" != "$RUN_IDENTITY" ]; then
  echo "results dir holds a different run:" >&2
  echo "  file:    $(cat "$IDENTITY_FILE")" >&2
  echo "  current: $RUN_IDENTITY" >&2
  echo "archive or empty $RESULTS, then re-run (or point RESULTS= at a fresh dir)" >&2
  exit 2
fi
printf '%s\n' "$RUN_IDENTITY" > "$IDENTITY_FILE"

DEPLOY_BACKUP="$(mktemp -t pixel-eval-prompt.XXXXXX)"
AGY_BACKUP="$(mktemp -t pixel-eval-agy.XXXXXX)"
restore() {
  if [ -s "$DEPLOY_BACKUP" ]; then
    cp "$DEPLOY_BACKUP" "$DEPLOY_PROMPT" 2>/dev/null || true
    rm -f "$DEPLOY_BACKUP"
  fi
  if [ -s "$AGY_BACKUP" ]; then
    while read -r name state; do
      if [ "$state" = disabled ]; then agy plugin disable "$name" >/dev/null 2>&1 || true
      else agy plugin enable "$name" >/dev/null 2>&1 || true; fi
    done < "$AGY_BACKUP"
    rm -f "$AGY_BACKUP"
  fi
}
trap restore EXIT

log() { printf '\n=== %s ===\n' "$*"; }

# --- sandbox builders -------------------------------------------------------
strip_pixel_hooks_py() { python3 "$EVAL_DIR/lib/strip_pixel_hooks.py"; }
merge_single_hook_set_py() {
  local mode="quiet"
  case "$1" in on|vslim|vminimal) mode="full" ;; esac
  python3 "$EVAL_DIR/lib/merge_single_hook_set.py" "$PIXEL_BIN" "$mode"
}
apply_agents_block_py() { python3 "$EVAL_DIR/lib/apply_agents_block.py" "$@"; }

build_arm() {
  local arm="$1" wt="$SCRATCH/wt-$arm" cfg="$SCRATCH/cfg-$arm"
  if [ ! -d "$wt" ]; then
    git -C "$REPO" worktree add --detach "$wt" "$EVAL_HEAD" >/dev/null 2>&1
  else
    git -C "$wt" reset --hard "$EVAL_HEAD" >/dev/null 2>&1
  fi
  rm -rf "$wt/.pixel"; cp -R "$MAIN_ROOT/.pixel" "$wt/.pixel"
  git -C "$wt" checkout -- AGENTS.md 2>/dev/null || true
  case "$arm" in
    baseline)
      (cd "$wt" && apply_agents_block_py strip)
      rm -rf "$wt/.agents/skills/pixel"
      ;;
    on)
      (cd "$wt" && apply_agents_block_py replace "$EVAL_DIR/variants/frozen-main/rules-body.md")
      ;;
    vquiet)
      (cd "$wt" && apply_agents_block_py replace "$EVAL_DIR/variants/slim/rules-body.md")
      ;;
    v*)
      local body="$EVAL_DIR/variants/${arm#v}/rules-body.md"
      local payload="$EVAL_DIR/variants/${arm#v}/agent-prompt.md"
      [ -f "$body" ] && [ -f "$payload" ] || { echo "variant ${arm#v} missing files" >&2; exit 2; }
      (cd "$wt" && apply_agents_block_py replace "$body")
      ;;
  esac
  # claude config: real settings minus every pixel hook; baseline stays clean,
  # on/variant get exactly one pixel set
  rm -rf "$cfg"; mkdir -p "$cfg"
  strip_pixel_hooks_py < "$HOME/.claude/settings.json" > "$cfg/settings.json"
  if [ "$arm" != "baseline" ]; then
    merge_single_hook_set_py "$arm" < "$cfg/settings.json" > "$cfg/settings.json.tmp" && mv "$cfg/settings.json.tmp" "$cfg/settings.json"
  fi
  [ -f "$HOME/.claude/CLAUDE.md" ] && cp "$HOME/.claude/CLAUDE.md" "$cfg/CLAUDE.md" || true
  for name in plugins skills agents commands context-mode advanced-memory; do
    [ -e "$HOME/.claude/$name" ] && [ ! -e "$cfg/$name" ] && ln -s "$HOME/.claude/$name" "$cfg/$name" || true
  done
  echo "built arm=$arm wt=$wt cfg=$cfg"
}

# --- CLI runners ------------------------------------------------------------
run_cli() {  # cli arm scenario outfile
  local cli="$1" arm="$2" scenario="$3" out="$4"
  local wt="$SCRATCH/wt-$arm" cfg="$SCRATCH/cfg-$arm"
  local prompt; prompt=$(python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "$EVAL_DIR/scenarios/$scenario.json")
  case "$cli" in
    claude)
      export CLAUDE_CONFIG_DIR="$cfg"
      (cd "$wt" && "${CLAUDE_BIN:-$HOME/.local/bin/claude}" -p "$prompt" \
        --output-format stream-json --verbose --max-turns "$MAX_TURNS" \
        --dangerously-skip-permissions > "$out" 2> "${out%.jsonl}.err")
      ;;
    agy)
      (cd "$wt" && agy -p "$prompt" --output-format stream-json --print-timeout 420s \
        > "$out" 2> "${out%.jsonl}.err")
      ;;
    codex|pi)
      echo "run_cli: $cli not implemented — see eval/README.md" >&2; return 9 ;;
  esac
}

swap_payload() {  # arm -> deploy the payload that arm's session-start hook should emit
  local arm="$1"
  [ -s "$DEPLOY_BACKUP" ] || cp "$DEPLOY_PROMPT" "$DEPLOY_BACKUP"
  case "$arm" in
    on) cp "$EVAL_DIR/variants/frozen-main/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vfinal) cp "$EVAL_DIR/variants/final/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vquiet|vslim) cp "$EVAL_DIR/variants/slim/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    vminimal) cp "$EVAL_DIR/variants/minimal/agent-prompt.md" "$DEPLOY_PROMPT" ;;
    baseline) : ;;   # no hooks run, payload irrelevant
  esac
}

agy_plugin_guard() {  # keep agy's global pixel plugin only for the `on` arm
  if [ "$CLIS" != claude ] && [[ "$CLIS" == *agy* ]]; then
    # Capture via tmp+mv: a failed `agy plugin list` must not leave an empty
    # file shadowing every later capture attempt (restore reads it with -s).
    if [ ! -s "$AGY_BACKUP" ]; then
      if agy plugin list 2>/dev/null | python3 -c '
import json,sys
try: d=json.load(sys.stdin)
except Exception: d={}
for i in d.get("imports",[]):
    if i.get("name")=="pixel": print("pixel", "enabled")' > "$AGY_BACKUP.tmp" && [ -s "$AGY_BACKUP.tmp" ]; then
        mv "$AGY_BACKUP.tmp" "$AGY_BACKUP"
      else
        rm -f "$AGY_BACKUP.tmp"
      fi
    fi
    if [ "$1" = off ]; then agy plugin disable pixel >/dev/null 2>&1 || true
    else agy plugin enable pixel >/dev/null 2>&1 || true; fi
  fi
}

# --- main loop --------------------------------------------------------------
for arm in $ARMS; do
  build_arm "$arm"
  swap_payload "$arm"
  agy_plugin_guard "$([ "$arm" = on ] && echo on || echo off)"
  for scenario in $SCENARIOS; do
    for cli in $CLIS; do
      out="$RESULTS/$scenario-$arm.$cli.jsonl"
      if [ -s "$out" ]; then log "skip $scenario/$arm/$cli (exists)"; continue; fi
      log "run  scenario=$scenario arm=$arm cli=$cli"
      run_cli "$cli" "$arm" "$scenario" "$out" || echo "run failed rc=$? ($scenario/$arm/$cli)"
    done
  done
done

log "scoring"
python3 "$EVAL_DIR/score.py" --results "$RESULTS" --scenarios-dir "$EVAL_DIR/scenarios" $ARMS
