#!/usr/bin/env python3
"""Score experiment runs: compute discovery_ops, distinct_files_read, bytes_read,
wall_seconds, gold_recall, gold_precision.

Reads artifacts/results.json and per-run shim logs + transcripts.
Writes artifacts/scored_results.json and prints a summary table.

Shim log format (tab-separated):
  timestamp\tcommand\targs_json\tstdout_bytes

Transcript parsing: extracts named files from the "FILES TO CHANGE:" section.
Also counts transcript-captured file reads as a secondary source (harness
built-in tools bypass the shim).
"""
import json
import os
import re
import sys
from pathlib import Path
from collections import defaultdict

SCRIPT_DIR = Path(__file__).resolve().parent
TASKS_FILE = SCRIPT_DIR / "tasks.json"
ARTIFACTS_DIR = SCRIPT_DIR / "artifacts"

# Commands counted as discovery operations
DISCOVERY_CMDS = {
    "ls", "find", "grep", "rg", "cat", "head", "tail",
    "sed", "awk", "tree", "wc", "gitpixel",
}

# Commands that read file content (for distinct_files_read and bytes_read)
FILE_READ_CMDS = {"cat", "head", "tail", "sed", "awk"}

# Commands that search (for discovery_ops but not file reads)
SEARCH_CMDS = {"ls", "find", "grep", "rg", "tree", "wc"}


def load_tasks():
    with open(TASKS_FILE) as f:
        data = json.load(f)
    return {t["id"]: t for t in data["tasks"]}


def parse_shim_log(shim_log_path):
    """Parse a shim log file. Returns list of (timestamp, cmd, args, bytes) tuples."""
    entries = []
    if not shim_log_path or not os.path.exists(shim_log_path):
        return entries
    with open(shim_log_path) as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            parts = line.split("\t", 3)
            if len(parts) < 4:
                # Malformed line, skip
                continue
            ts, cmd, args_json, bytes_str = parts
            try:
                args = json.loads(args_json)
            except (json.JSONDecodeError, ValueError):
                args = args_json
            try:
                out_bytes = int(bytes_str)
            except ValueError:
                out_bytes = 0
            entries.append((float(ts), cmd, args, out_bytes))
    return entries


def extract_file_paths_from_args(cmd, args):
    """Extract file paths from command arguments."""
    if isinstance(args, str):
        args_list = args.split()
    elif isinstance(args, list):
        args_list = args
    else:
        args_list = str(args).split()

    paths = []
    if cmd in FILE_READ_CMDS:
        # For cat/head/tail, non-flag args are file paths
        for arg in args_list:
            if not arg.startswith("-") and arg != "":
                # Skip stdin marker
                if arg == "-":
                    continue
                paths.append(arg)
    elif cmd in ("grep", "rg"):
        # For grep/rg, the pattern is first, paths follow
        # Skip flags and the first non-flag arg (pattern)
        found_pattern = False
        for arg in args_list:
            if arg.startswith("-"):
                continue
            if not found_pattern:
                found_pattern = True
                continue
            if arg != "-" and arg != ".":
                paths.append(arg)
    return paths


def compute_shim_metrics(entries):
    """Compute discovery_ops, distinct_files_read, bytes_read from shim entries."""
    discovery_ops = 0
    files_read = set()
    bytes_read = 0

    for ts, cmd, args, out_bytes in entries:
        if cmd in DISCOVERY_CMDS:
            discovery_ops += 1

        if cmd in FILE_READ_CMDS:
            paths = extract_file_paths_from_args(cmd, args)
            for p in paths:
                files_read.add(p)
            bytes_read += out_bytes
        elif cmd in ("grep", "rg"):
            # grep/rg output is also bytes the agent pulled in
            bytes_read += out_bytes

    return {
        "discovery_ops": discovery_ops,
        "distinct_files_read": len(files_read),
        "files_read_list": sorted(files_read),
        "bytes_read": bytes_read,
    }


def extract_named_files(transcript_path):
    """Extract named files from the FILES TO CHANGE section of the transcript."""
    if not transcript_path or not os.path.exists(transcript_path):
        return []

    with open(transcript_path) as f:
        content = f.read()

    # Find the FILES TO CHANGE section
    # Look for the section header (case-insensitive, flexible)
    patterns = [
        r"FILES TO CHANGE:\s*\n((?:.|\n)*)",
        r"FILES TO CHANGE\s*:?\s*\n((?:.|\n)*)",
        r"files to change\s*:?\s*\n((?:.|\n)*)",
    ]

    section_content = None
    for pat in patterns:
        m = re.search(pat, content, re.IGNORECASE)
        if m:
            section_content = m.group(1)
            break

    if not section_content:
        # Fallback: look for a list of file paths near the end
        # Try to find lines that look like file paths
        lines = content.split("\n")
        file_paths = []
        for line in lines:
            line = line.strip().strip("`").strip("*").strip("-").strip()
            # Match common source file patterns
            if re.match(r'^[\w/.-]+\.(rs|ts|js|tsx|jsx|py|go|java|toml|json|md|yaml|yml|sh)$', line):
                file_paths.append(line)
        return file_paths

    # Parse file paths from the section
    file_paths = []
    for line in section_content.split("\n"):
        line = line.strip()
        if not line:
            continue
        # Stop at section breaks or end of content
        if line.startswith("===") or line.startswith("---"):
            break
        # Clean up common formatting
        line = re.sub(r'^[\d]+[\.\)]\s*', '', line)  # numbered list
        line = line.strip("`").strip("*").strip("-").strip("•").strip()
        line = line.strip()

        if not line:
            continue

        # Extract file path (may have description after it)
        # Try to match a path-like string
        m = re.match(r'^([\w/.-]+\.(rs|ts|js|tsx|jsx|py|go|java|toml|json|md|yaml|yml|sh|lock|gitignore))', line)
        if m:
            file_paths.append(m.group(1))
        elif re.match(r'^[\w/][\w/.-]*$', line) and ("/" in line or "." in line):
            # Path-like without known extension (e.g., .gitignore, Cargo.lock)
            file_paths.append(line)

    return file_paths


def normalize_path(path):
    """Normalize a file path for comparison.

    Handles:
    - Leading ./
    - Absolute paths (strips prefix to repo-relative)
    - Backticks, asterisks, numbering
    """
    path = path.strip().strip("`").strip("*").strip()
    # Remove numbered list prefix
    path = re.sub(r'^\d+[\.\)]\s*', '', path)
    if path.startswith("./"):
        path = path[2:]
    # If absolute path, try to extract repo-relative part
    if path.startswith("/"):
        # Look for known top-level dirs in this repo
        for topdir in ["crates/", "js/", "docs/", "target/", "Cargo", "README",
                       "NOTICE", ".gitignore", "scripts/"]:
            idx = path.find("/" + topdir if topdir != "Cargo" and topdir != "README"
                            and topdir != "NOTICE" and topdir != ".gitignore"
                            else topdir)
            if idx >= 0:
                # For dirs like crates/, js/, etc., the match includes a leading /
                # For files like Cargo.toml, README.md, etc., find the last occurrence
                if topdir.endswith("/"):
                    path = path[idx + 1:]  # skip leading /
                else:
                    # File at root: take from the match to end
                    path = path[idx:]
                break
        else:
            # No known top-level dir found, try to strip common prefixes
            parts = path.split("/")
            # Try to find a part that looks like a repo dir
            for i, part in enumerate(parts):
                if part in ("crates", "js", "docs", "scripts", "target"):
                    path = "/".join(parts[i:])
                    break
    return path


def compute_gold_metrics(named_files, gold_files):
    """Compute gold_recall and gold_precision."""
    named_set = set(normalize_path(f) for f in named_files)
    gold_set = set(normalize_path(f) for f in gold_files)

    if not gold_set:
        return {"gold_recall": 0.0, "gold_precision": 0.0, "intersection": []}

    intersection = named_set & gold_set
    recall = len(intersection) / len(gold_set) if gold_set else 0.0
    precision = len(intersection) / len(named_set) if named_set else 0.0

    return {
        "gold_recall": round(recall, 4),
        "gold_precision": round(precision, 4),
        "intersection": sorted(intersection),
        "named_count": len(named_set),
        "gold_count": len(gold_set),
    }


def count_transcript_file_reads(transcript_path):
    """Count file read operations from the transcript (secondary source).

    Harnesses with built-in file tools (gemini ReadFile, opencode internal read)
    bypass the shim. This is a best-effort parse of the transcript.
    """
    if not transcript_path or not os.path.exists(transcript_path):
        return {"transcript_reads": 0, "transcript_files": []}

    with open(transcript_path) as f:
        content = f.read()

    files = set()

    # Gemini patterns: ReadFile(path), reading file, etc.
    for m in re.finditer(r'ReadFile\s*\(\s*["\']?([^"\'\)]+)["\']?\s*\)', content, re.IGNORECASE):
        files.add(m.group(1).strip())
    for m in re.finditer(r'reading\s+(?:file\s+)?["\']?([\w/][\w/.-]+)["\']?', content, re.IGNORECASE):
        files.add(m.group(1).strip())

    # OpenCode patterns: read_file, readFile, etc.
    for m in re.finditer(r'read_file\s*\(\s*["\']?([^"\'\)]+)["\']?\s*\)', content, re.IGNORECASE):
        files.add(m.group(1).strip())

    # Codex patterns: read_file, cat, etc. in tool call logs
    for m in re.finditer(r'"path"\s*:\s*"([^"]+)"', content):
        p = m.group(1).strip()
        if "/" in p or "." in p:
            files.add(p)

    # Generic: lines that mention reading a file
    for m in re.finditer(r'(?:reading|read|opened|opening)\s+(?:file\s+)?[`"]?([\w/][\w/.-]+\.\w+)[`"]?', content, re.IGNORECASE):
        files.add(m.group(1).strip())

    return {
        "transcript_reads": len(files),
        "transcript_files": sorted(files),
    }


def main():
    results_path = ARTIFACTS_DIR / "results.json"
    if not results_path.exists():
        print("No results.json found. Run the experiment first.", file=sys.stderr)
        sys.exit(1)

    with open(results_path) as f:
        results = json.load(f)

    tasks = load_tasks()

    scored = []
    for r in results:
        if r["status"].startswith("dropped"):
            scored.append(r)
            continue

        task_id = r["task_id"]
        task = tasks.get(task_id)
        if not task:
            r["score_error"] = "task not found"
            scored.append(r)
            continue

        # Shim metrics
        shim_entries = parse_shim_log(r.get("shim_log_path"))
        shim_metrics = compute_shim_metrics(shim_entries)

        # Named files from transcript
        named_files = extract_named_files(r.get("transcript_path"))

        # Gold metrics
        gold_metrics = compute_gold_metrics(named_files, task["gold_files"])

        # Transcript file reads (secondary source)
        transcript_metrics = count_transcript_file_reads(r.get("transcript_path"))

        scored.append({
            "run_id": r["run_id"],
            "task_id": task_id,
            "arm": r["arm"],
            "harness": r["harness"],
            "status": r["status"],
            "wall_seconds": r.get("wall_seconds"),
            "index_build_seconds": r.get("index_build_seconds"),
            "discovery_ops": shim_metrics["discovery_ops"],
            "distinct_files_read": shim_metrics["distinct_files_read"],
            "files_read_list": shim_metrics["files_read_list"],
            "bytes_read": shim_metrics["bytes_read"],
            "named_files": named_files,
            "named_count": gold_metrics["named_count"],
            "gold_count": gold_metrics["gold_count"],
            "gold_recall": gold_metrics["gold_recall"],
            "gold_precision": gold_metrics["gold_precision"],
            "intersection": gold_metrics["intersection"],
            "transcript_reads": transcript_metrics["transcript_reads"],
            "transcript_files": transcript_metrics["transcript_files"],
            "edits_detected": r.get("edits_detected", False),
            "drop_reason": r.get("drop_reason"),
        })

    # Save scored results
    scored_path = ARTIFACTS_DIR / "scored_results.json"
    with open(scored_path, "w") as f:
        json.dump(scored, f, indent=2)

    # Print summary table
    print("\n=== SCORED RESULTS ===\n")
    header = f"{'run_id':<25} {'status':<25} {'disc_ops':>8} {'files':>6} {'bytes':>8} {'wall_s':>7} {'recall':>7} {'prec':>7} {'t_reads':>8}"
    print(header)
    print("-" * len(header))
    for r in scored:
        if r["status"].startswith("dropped"):
            print(f"{r['run_id']:<25} {r['status']:<25} {'-':>8} {'-':>6} {'-':>8} {'-':>7} {'-':>7} {'-':>7} {'-':>8}")
        else:
            print(f"{r['run_id']:<25} {r['status']:<25} {r.get('discovery_ops','-'):>8} "
                  f"{r.get('distinct_files_read','-'):>6} {r.get('bytes_read','-'):>8} "
                  f"{r.get('wall_seconds','-'):>7} {r.get('gold_recall','-'):>7} "
                  f"{r.get('gold_precision','-'):>7} {r.get('transcript_reads','-'):>8}")

    # Per-harness per-arm medians
    print("\n=== PER-CELL MEDIANS (single trial — no variance estimate) ===\n")
    groups = defaultdict(list)
    for r in scored:
        if r["status"].startswith("dropped"):
            continue
        key = (r["harness"], r["arm"])
        groups[key].append(r)

    print(f"{'harness':<12} {'arm':<5} {'n':>3} {'disc_ops':>8} {'files':>6} {'bytes':>8} {'wall_s':>7} {'recall':>7} {'prec':>7}")
    print("-" * 70)
    for (harness, arm) in sorted(groups.keys()):
        runs = groups[(harness, arm)]
        n = len(runs)
        medians = {}
        for metric in ["discovery_ops", "distinct_files_read", "bytes_read", "wall_seconds", "gold_recall", "gold_precision"]:
            vals = [r[metric] for r in runs if r.get(metric) is not None]
            if vals:
                vals.sort()
                medians[metric] = vals[len(vals) // 2]
            else:
                medians[metric] = "-"
        print(f"{harness:<12} {arm:<5} {n:>3} {medians['discovery_ops']:>8} {medians['distinct_files_read']:>6} "
              f"{medians['bytes_read']:>8} {medians['wall_seconds']:>7} {medians['gold_recall']:>7} {medians['gold_precision']:>7}")

    # Overall per-arm medians
    print("\n=== OVERALL PER-ARM MEDIANS ===\n")
    for arm in ["A", "B", "C", "D"]:
        runs = [r for r in scored if r.get("arm") == arm and not r["status"].startswith("dropped")]
        if not runs:
            continue
        medians = {}
        for metric in ["discovery_ops", "distinct_files_read", "bytes_read", "wall_seconds", "gold_recall", "gold_precision"]:
            vals = [r[metric] for r in runs if r.get(metric) is not None]
            if vals:
                vals.sort()
                medians[metric] = vals[len(vals) // 2]
            else:
                medians[metric] = "-"
        print(f"Arm {arm} (n={len(runs)}): disc_ops={medians['discovery_ops']} files={medians['distinct_files_read']} "
              f"bytes={medians['bytes_read']} wall={medians['wall_seconds']} recall={medians['gold_recall']} prec={medians['gold_precision']}")

    print(f"\nScored results saved to {scored_path}")


if __name__ == "__main__":
    main()
