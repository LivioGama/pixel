#!/usr/bin/env bash
# eval/arena.sh — head-to-head: raw codex vs retrieval-tool arms, ranked.
#
# Arms: raw | semble | graft | stacklit | gitnexus | gortex | pixel
# Each arm = one docker image (eval/arena/Dockerfile.<arm>, FROM the shared
# pixel-arena-base) with its tool installed and wired to codex per that tool's
# own codex docs. The repo snapshot, auth, model, sandbox, and prompts are
# identical across arms.
#
# Usage: eval/arena.sh [--arms "raw pixel"] [--tasks "s1 s2 s3"] [--reps N]
# Results: eval/arena-results/<arm>-<task>-<rep>.jsonl + rank table.
set -euo pipefail
ARENA_DIR="$(cd "$(dirname "$0")" && pwd)"
DOCKER="$_"   # placeholder; resolved below to bypass shell wrappers
DOCKER_BIN="$(command -v docker)"
REPO_SNAPSHOT="${REPO_SNAPSHOT:?set REPO_SNAPSHOT to the repo dir to mount at /repo}"
AUTH="${AUTH:-$HOME/.codex/auth.json}"
ARMS="${ARMS:-raw semble graft stacklit gitnexus gortex pixel}"
TASKS="${TASKS:-s1-hook-install s2-vector-recall s3-rename-impact}"
REPS="${REPS:-1}"
RESULTS="$ARENA_DIR/arena-results"
mkdir -p "$RESULTS"
START=$(date +%s)

# flags override env: --arms, --tasks, --reps
while [ $# -gt 0 ]; do
  case "$1" in
    --arms) ARMS="$2"; shift 2 ;;
    --tasks) TASKS="$2"; shift 2 ;;
    --reps) REPS="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

prompt_for() { python3 -c "import json,sys;print(json.load(open(sys.argv[1]))['prompt'])" "$ARENA_DIR/scenarios/$1.json"; }

run_arm_task() {  # arm task rep
  local arm="$1" task="$2" rep="$3"
  local out="$RESULTS/$arm-$task-$rep.jsonl"
  local t0 t1
  if [ -s "$out" ]; then echo "skip $arm/$task/$rep (exists)"; return 0; fi
  local prompt; prompt=$(prompt_for "$task")
  echo ">>> $arm / $task / rep$rep"
  touch "$out"   # docker -v creates missing host paths as directories otherwise
  # per-arm rw snapshot: arms write their indexes (.pixel/, graft/, ...) into
  # the repo, and one arm's artifacts must not leak into the next arm's runs
  local snap="$RESULTS/snapshot-$arm"
  if [ ! -d "$snap" ]; then
    git clone -q "$REPO_SNAPSHOT" "$snap"
  fi
  t0=$(date +%s)
  "$DOCKER_BIN" run --rm \
    -v "$snap":/repo \
    -v "$AUTH":/root/.codex/auth.json:ro \
    -v "$out":/out.jsonl \
    -e PROMPT="$prompt" -e OUT=/out.jsonl \
    "pixel-arena:$arm" \
    || { echo "RUN FAILED rc=$? ($arm/$task/$rep)"; return 0; }
  t1=$(date +%s)
  echo $((t1 - t0)) > "${out%.jsonl}.seconds"
  echo "    done in $((t1 - t0))s"
}

for arm in $ARMS; do
  docker_build="pixel-arena:$arm"
  if ! "$DOCKER_BIN" image inspect "$docker_build" >/dev/null 2>&1; then
    echo "=== building image $docker_build"
    "$DOCKER_BIN" build -f "$ARENA_DIR/arena/Dockerfile.$arm" -t "$docker_build" "$ARENA_DIR" || { echo "IMAGE BUILD FAILED: $arm"; exit 1; }
  fi
done

for rep in $(seq 1 "$REPS"); do
  for task in $TASKS; do
    for arm in $ARMS; do
      run_arm_task "$arm" "$task" "$rep"
    done
  done
done

echo "=== ranking"
python3 "$ARENA_DIR/arena/rank.py" --results "$RESULTS" --scenarios-dir "$ARENA_DIR/scenarios" --arms $ARMS
echo "total wall: $(( $(date +%s) - START ))s"
