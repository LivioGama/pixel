#!/bin/bash
# Re-run the commits whose checkout failed: before #307, ensure_pixel_gitignored
# appends `.pixel/` to a tracked .gitignore that lacks it, which blocks the
# next checkout. --force discards that edit (the .pixel/ dir is untracked).
# Usage: rerun.sh <name> <tree>
set -u
name=$1 tree=$2
BIN=${PIXEL_BIN:-pixel}
out=$(dirname "$0")/out/$name
[ -f "$out/errors.txt" ] || exit 0
mv "$out/errors.txt" "$out/errors.first-pass.txt"
cd "$tree" || exit 1
while read -r i c _; do
  git checkout -q -f --detach "$c" || { echo "$i $c checkout-failed" >> "$out/errors.txt"; continue; }
  f="$out/$(printf %03d "$i")-$c"
  start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  PIXEL_METRICS=0 "$BIN" what-changed --base "$c^" --json . > "$f.json" 2> "$f.err"
  rc=$?
  end=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  echo "$i $c rc=$rc secs=$(echo "$end - $start" | bc) rerun" >> "$out/progress.txt"
done < "$out/errors.first-pass.txt"
git checkout -q -f --detach "$(tail -1 "$out/commits.txt")"
"$BIN" daemon stop . >/dev/null 2>&1 || true
echo "rerun done $name"
