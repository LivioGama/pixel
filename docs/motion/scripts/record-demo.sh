#!/usr/bin/env bash
# Records the agent runs the AgentDemo composition replays.
#
# Both arms run the same model, at the same effort, on the same task, in a
# throwaway worktree of a pinned ref with docs/motion removed (so no agent
# can read the demo's own traces or this script). Both start from the same
# bare configuration: no settings sources (so no CLAUDE.md, rules, hooks or
# permissions from any settings file), no skills, no MCP server, no
# sub-agents and no editing tools, and both are told to answer in English.
#
# The only difference: the `pixel` arm gets the hooks `pixel install` writes
# for Claude Code (SessionStart injects the agent prompt, UserPromptSubmit
# adds the task packet, PostToolUse records metrics), passed with
# --settings, so it measures Pixel as a user installs it.
#
# Usage: [ARMS="vanilla pixel"] [PAR=n] [REF=v0.5.0] [EFFORT=medium] \
#          scripts/record-demo.sh <out-dir> [reps] [model]
# Both arms of a rep always start together; PAR caps how many reps run at
# once (default: all), so a rate limit or a busy machine hits both arms alike.
# Writes <arm>-<rep>.jsonl: one stream-json event per line, each wrapped as
# {"t": <ms since epoch when the line arrived>, "e": <event>}.
set -euo pipefail

out=${1:?out dir}
reps=${2:-3}
model=${3:-opus}
effort=${EFFORT:-medium}
ref=${REF:-v0.5.0}
source_repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
pixel_bin=$(command -v pixel)
prompt_file=$HOME/.local/share/pixel/agent-prompt.md
task='Task: retry a leased push when the remote branch moved. Before anyone edits anything, find the files that would need to change and list them, most important first, with one line each on why. Do not edit any file.'
english='Always write your final answer in English.'

mkdir -p "$out"
out=$(cd "$out" && pwd)

# The repository both arms work in, outside the source tree, indexed before
# any clock starts.
work=$(mktemp -d)
repo="$work/pixel"
trap 'git -C "$source_repo" worktree remove --force "$repo" >/dev/null 2>&1 || true; rm -rf "$work"' EXIT
git -C "$source_repo" worktree add --detach --quiet "$repo" "$ref"
rm -rf "$repo/docs/motion"
git -C "$repo" add -A
git -C "$repo" -c user.name=demo -c user.email=demo@localhost commit --quiet -m "demo: drop the demo's own sources"
(cd "$repo" && pixel prepare-repo . >/dev/null 2>&1)

settings=$(cat <<JSON
{"hooks": {
  "SessionStart": [{"hooks": [{"type": "command", "command": "'$pixel_bin' run-hook session-start"}]}],
  "UserPromptSubmit": [{"hooks": [{"type": "command", "command": "'$pixel_bin' run-hook prompt-submit --provider claude"}]}],
  "PostToolUse": [{"matcher": "Edit", "hooks": [{"type": "command", "command": "'$pixel_bin' run-hook post-tool-use --provider claude"}]}]
}}
JSON
)

{
  echo "ref=$ref ($(git -C "$source_repo" rev-parse "$ref^{commit}"))"
  echo "model=$model effort=$effort"
  echo "claude=$(claude --version)"
  echo "pixel=$("$pixel_bin" --version | head -2 | tr '\n' ' ')"
  echo "prompt_sha256=$(shasum -a 256 "$prompt_file" | cut -d' ' -f1)"
  echo "task=$task"
  echo "arms=${ARMS:-vanilla pixel} reps=$reps started=$(date -u +%FT%TZ)"
} > "$out/meta.txt"

stamp() { python3 -u -c '
import sys, json, time
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        e = json.loads(line)
    except ValueError:
        continue
    print(json.dumps({"t": int(time.time() * 1000), "e": e}), flush=True)
'; }

run() {
  local arm=$1 rep=$2
  local extra=()
  if [ "$arm" = pixel ]; then extra=(--settings "$settings"); fi
  (cd "$repo" && env -u CLAUDECODE claude -p "$task" \
    --model "$model" --effort "$effort" \
    --setting-sources "" --disable-slash-commands --strict-mcp-config \
    --append-system-prompt "$english" \
    --allowedTools "Bash Read Grep Glob" \
    --disallowedTools "Edit Write NotebookEdit Agent Task WebFetch WebSearch" \
    --output-format stream-json --verbose \
    ${extra[@]+"${extra[@]}"} < /dev/null) | stamp > "$out/$arm-$rep.jsonl"
}

par=${PAR:-$reps}
for rep in $(seq 1 "$reps"); do
  for arm in ${ARMS:-vanilla pixel}; do run "$arm" "$rep" & done
  if [ $((rep % par)) -eq 0 ]; then wait; fi
done
wait
