#!/bin/bash
# Re-run the commits whose checkout failed in run.sh. Before #307 the pixel
# under test appended `.pixel/` to a tracked .gitignore that lacked it, and
# that edit blocked the next checkout; common.sh undoes exactly that edit,
# and refuses any other.
# Usage: rerun.sh <name> <tree>
set -u
name=$1 tree=$2
BIN=${PIXEL_BIN:-pixel}
. "$(dirname "$0")/common.sh"
out=$(out_dir "$name")
[ -f "$out/errors.txt" ] || exit 0
cd "$tree" || exit 1
require_clean || exit 1
start_rev=$(git rev-parse HEAD)
mv "$out/errors.txt" "$out/errors.first-pass.txt"
while read -r i c _; do
  measure "$out" "$i" "$c" rerun
done < "$out/errors.first-pass.txt"
restore "$start_rev"
"$BIN" daemon stop . >/dev/null 2>&1 || true
echo "rerun done $name"
