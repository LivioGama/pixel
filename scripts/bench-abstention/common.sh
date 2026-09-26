# shellcheck shell=bash
# Shared by run.sh and rerun.sh. Sourced, not executed.

# Absolute output directory for <name>, whatever the caller's directory.
out_dir() {
  echo "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/out/$1"
}

# Before #307 the pixel under test appended `.pixel/` to a tracked
# .gitignore that lacked it. Undo exactly that edit and nothing else: a
# .gitignore whose only change is one added `.pixel/` line.
drop_pixel_gitignore_edit() {
  [ "$(git diff --numstat -- .gitignore)" = "$(printf '1\t0\t.gitignore')" ] || return 0
  if git diff -U0 -- .gitignore | grep -qx '+\.pixel/'; then
    git checkout -q -- .gitignore
  fi
}

# Refuse a tree with tracked edits: they would ride along into every
# measured diff, and a checkout could lose them.
require_clean() {
  drop_pixel_gitignore_edit
  if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
    echo "refusing: $(pwd) has tracked edits" >&2
    git status --short --untracked-files=no >&2
    return 1
  fi
}

# Measure one commit: check it out, then map its diff onto the refreshed
# graph. Appends one line to progress.txt, or to errors.txt on checkout
# failure. Usage: measure <out> <index> <commit> [tag]
measure() {
  local out=$1 i=$2 c=$3 tag=${4:-}
  drop_pixel_gitignore_edit
  if ! git cat-file -e "$c^" 2>/dev/null; then
    # A shallow clone's boundary: no base, so no diff to measure.
    echo "$i $c no-parent" >> "$out/errors.txt"
    return
  fi
  if ! git checkout -q --detach "$c"; then
    echo "$i $c checkout-failed" >> "$out/errors.txt"
    return
  fi
  local f start end rc
  f="$out/$(printf %03d "$i")-$c"
  start=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  PIXEL_METRICS=0 "$BIN" what-changed --base "$c^" --json . > "$f.json" 2> "$f.err"
  rc=$?
  end=$(perl -MTime::HiRes=time -e 'printf "%.3f", time')
  echo "$i $c rc=$rc secs=$(echo "$end - $start" | bc)${tag:+ $tag}" >> "$out/progress.txt"
}

# Put the tree back at <rev> and say so if that failed.
restore() {
  drop_pixel_gitignore_edit
  if ! git checkout -q --detach "$1" || [ "$(git rev-parse HEAD)" != "$(git rev-parse "$1")" ]; then
    echo "warning: $(pwd) was not restored to $1" >&2
    return 1
  fi
}
