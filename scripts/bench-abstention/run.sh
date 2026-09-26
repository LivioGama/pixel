#!/bin/bash
# d2-5 — abstention rate of a whole-change negative, per motif.
# Usage: run.sh <name> <tree> <n>   (then: aggregate.py <name>=<clone> ...)
#   <tree> is a checkout this script may move (a dedicated worktree or clone).
# For each of the last <n> non-merge commits of HEAD, oldest first: check the
# commit out, then `what-changed --base <commit>^ --json`, which refreshes the
# graph to the working tree and maps the commit's diff onto it.
set -u
name=$1 tree=$2 n=$3
BIN=${PIXEL_BIN:-pixel}
out=$(dirname "$0")/out/$name
mkdir -p "$out"
cd "$tree" || exit 1
tip=$(git rev-parse HEAD)
git rev-list --no-merges -n "$n" "$tip" | tail -r > "$out/commits.txt"
i=0
while read -r c; do
  i=$((i + 1))
  git checkout -q --detach "$c" || { echo "$i $c checkout-failed" >> "$out/errors.txt"; continue; }
  start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  PIXEL_METRICS=0 "$BIN" what-changed --base "$c^" --json . > "$out/$(printf %03d $i)-$c.json" 2> "$out/$(printf %03d $i)-$c.err"
  rc=$?
  end=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  echo "$i $c rc=$rc secs=$(echo "$end - $start" | bc)" >> "$out/progress.txt"
done < "$out/commits.txt"
git checkout -q --detach "$tip"
"$BIN" daemon stop . >/dev/null 2>&1 || true
echo "done $name"
