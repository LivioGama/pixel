#!/usr/bin/env bash
# Records the agent runs the AgentDemo composition replays.
#
# Both arms run the same model on the same task from the repository root,
# with the same bare configuration: no settings sources (so no CLAUDE.md,
# rules, hooks or permissions from any settings file), no skills, no MCP
# server, no sub-agents and no editing tools. The only difference is that
# the `pixel` arm gets the agent prompt `pixel install` deploys, appended to
# the system prompt the way the SessionStart hook injects it.
#
# Usage: [ARMS="vanilla pixel"] [PAR=n] scripts/record-demo.sh <out-dir> [reps] [model]
# Both arms of a rep always start together; PAR caps how many reps run at
# once (default: all), so a rate limit or a busy machine hits both arms alike.
# Writes <arm>-<rep>.jsonl: one stream-json event per line, each wrapped as
# {"t": <ms since epoch when the line arrived>, "e": <event>}.
set -euo pipefail

out=${1:?out dir}
reps=${2:-3}
model=${3:-sonnet}
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
prompt_file=${PIXEL_AGENT_PROMPT:-$HOME/.local/share/pixel/agent-prompt.md}
task='Task: retry a leased push when the remote branch moved. Before anyone edits anything, find the files that would need to change and list them, most important first, with one line each on why. Do not edit any file. Answer in English.'

mkdir -p "$out"
{
  echo "repo_head=$(git -C "$repo" rev-parse HEAD)"
  echo "model=$model"
  echo "claude=$(claude --version)"
  echo "pixel=$(pixel --version)"
  echo "prompt_sha256=$(shasum -a 256 "$prompt_file" | cut -d' ' -f1)"
  echo "task=$task"
  echo "arms=${ARMS:-vanilla pixel} started=$(date -u +%FT%TZ)"
} >> "$out/meta.txt"

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
  if [ "$arm" = pixel ]; then extra=(--append-system-prompt "$(cat "$prompt_file")"); fi
  (cd "$repo" && env -u CLAUDECODE claude -p "$task" \
    --model "$model" \
    --setting-sources "" --disable-slash-commands --strict-mcp-config \
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
