#!/usr/bin/env python3
"""Generate PATH shim wrappers for discovery-operation instrumentation.

Each wrapper appends a tab-separated record to $SHIM_LOG and then execs the
real binary. Format:
  timestamp\tcommand\targs_json\tstdout_bytes

For read commands (cat, head, tail), stdout_bytes is the actual output size.
For others, it is the output size too (may be 0 for no output).

Usage:
    python3 gen_shim.py /path/to/shim_dir /path/to/real/gitpixel
"""
import json
import os
import shutil
import stat
import sys

# Commands to wrap (discovery operations per the experiment design)
SHIM_COMMANDS = [
    "ls", "find", "grep", "rg", "cat", "head", "tail",
    "sed", "awk", "tree", "wc",
]

def make_shim_wrapper(cmd, real_path, real_cat, real_wc):
    """Build a bash shim wrapper that logs to $SHIM_LOG and execs the real binary.

    Format: timestamp\tcommand\targs\tstdout_bytes
    Uses real paths for internal cat/wc to avoid recursion.
    """
    return f'''#!/bin/bash
# Auto-generated PATH shim for experiment instrumentation.
# Logs to $SHIM_LOG, then execs the real binary.
_real="{real_path}"
if [ -z "$SHIM_LOG" ]; then
  exec "$_real" "$@"
fi
_ts=$(date +%s.%N)
_escaped_args=$(printf '%s ' "$@" | tr '\\t' ' ' | tr '\\n' ' ')
_tmpf=$(mktemp)
"$_real" "$@" 2>/dev/null > "$_tmpf"
_exit=$?
_bytes=$({real_wc} -c < "$_tmpf" 2>/dev/null || echo 0)
printf '%s\\t{cmd}\\t%s\\t%s\\n' "$_ts" "$_escaped_args" "$_bytes" >> "$SHIM_LOG"
{real_cat} "$_tmpf"
rm -f "$_tmpf"
exit $_exit
'''


def make_gitpixel_shim(real_path, real_cat, real_wc):
    """Build a gitpixel shim wrapper (captures stderr too)."""
    return f'''#!/bin/bash
# Auto-generated PATH shim for gitpixel instrumentation.
_real="{real_path}"
if [ -z "$SHIM_LOG" ]; then
  exec "$_real" "$@"
fi
_ts=$(date +%s.%N)
_escaped_args=$(printf '%s ' "$@" | tr '\\t' ' ' | tr '\\n' ' ')
_tmpf=$(mktemp)
"$_real" "$@" > "$_tmpf" 2>&1
_exit=$?
_bytes=$({real_wc} -c < "$_tmpf" 2>/dev/null || echo 0)
printf '%s\\tgitpixel\\t%s\\t%s\\n' "$_ts" "$_escaped_args" "$_bytes" >> "$SHIM_LOG"
{real_cat} "$_tmpf"
rm -f "$_tmpf"
exit $_exit
'''


def find_real_binary(cmd):
    """Find the real path of a command, excluding the shim dir itself."""
    path_dirs = os.environ.get("PATH", "").split(os.pathsep)
    for d in path_dirs:
        if d and os.path.isdir(d):
            candidate = os.path.join(d, cmd)
            if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
                # Make sure it's not a shim we already generated
                try:
                    with open(candidate) as f:
                        first_line = f.readline()
                    if "Auto-generated PATH shim" in first_line:
                        continue
                except Exception:
                    pass
                return os.path.realpath(candidate)
    return shutil.which(cmd)


def main():
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <shim_dir> <real_gitpixel_path>", file=sys.stderr)
        sys.exit(1)

    shim_dir = sys.argv[1]
    real_gitpixel = os.path.realpath(sys.argv[2])
    os.makedirs(shim_dir, exist_ok=True)

    # Resolve real paths for internal commands (avoid recursion)
    real_cat = find_real_binary("cat") or "/usr/bin/cat"
    real_wc = find_real_binary("wc") or "/usr/bin/wc"

    generated = []

    for cmd in SHIM_COMMANDS:
        real = find_real_binary(cmd)
        if not real:
            # Create a wrapper that logs and exits with "command not found"
            wrapper = f'''#!/bin/bash
# Auto-generated PATH shim for {cmd} (real binary not found).
if [ -n "$SHIM_LOG" ]; then
  _ts=$(date +%s.%N)
  printf '%s\\t{cmd}\\t""\\t0\\n' "$_ts" >> "$SHIM_LOG"
fi
echo "{cmd}: command not found" >&2
exit 127
'''
            path = os.path.join(shim_dir, cmd)
            with open(path, "w") as f:
                f.write(wrapper)
            os.chmod(path, 0o755)
            generated.append(f"{cmd} -> (not found)")
            continue

        wrapper = make_shim_wrapper(cmd, real, real_cat, real_wc)
        path = os.path.join(shim_dir, cmd)
        with open(path, "w") as f:
            f.write(wrapper)
        os.chmod(path, stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
        generated.append(f"{cmd} -> {real}")

    # gitpixel wrapper
    wrapper = make_gitpixel_shim(real_gitpixel, real_cat, real_wc)
    path = os.path.join(shim_dir, "gitpixel")
    with open(path, "w") as f:
        f.write(wrapper)
    os.chmod(path, stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
    generated.append(f"gitpixel -> {real_gitpixel}")

    print(f"Generated {len(generated)} shim wrappers in {shim_dir}:")
    for g in generated:
        print(f"  {g}")


if __name__ == "__main__":
    main()
