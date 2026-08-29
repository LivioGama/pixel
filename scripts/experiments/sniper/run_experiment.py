#!/usr/bin/env python3
"""Runner for the sniper-discovery experiment.

Executes the full matrix: 8 tasks × 3 harnesses × 2 arms = 48 runs,
plus 3 warmup runs that are discarded.

Design constraints (from docs/experiments/sniper-discovery.md):
- Localization only: agents name files, they never edit.
- At most 4 agent processes concurrently.
- Arm B runs gitpixel index + graph before the timer starts.
- Every dropped run is logged with its reason.
- Single trial per cell (no variance estimate).

Approach: tasks run sequentially (each needs a specific checkout in the test
repo). Within each task, the 6 cells (3 harnesses × 2 arms) run with max 4
concurrent. This avoids checkout conflicts while respecting the concurrency
limit.

Usage:
    python3 run_experiment.py [--warmup] [--max-concurrent N] [--harnesses h1,h2]
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

# Paths
SCRIPT_DIR = Path(__file__).resolve().parent
TASKS_FILE = SCRIPT_DIR / "tasks.json"
WORKING_REPO = Path.home() / "gitpixel"
TEST_REPO = Path.home() / "gitpixel-under-test"
GITPIXEL_BIN = WORKING_REPO / "target" / "release" / "gitpixel"
ARTIFACTS_DIR = SCRIPT_DIR / "artifacts"
SHIM_DIR = SCRIPT_DIR / "shim"

# Harness configurations
# NOTE: gemini --yolo caused hangs on a2 (agent stuck in tool-call loops);
# --approval-mode plan (read-only) works and is appropriate for a localization task.
# opencode uses its default model (deepseek-v4-pro on Mac) for genuine model diversity
# across the three harnesses. codex needs --skip-git-repo-check for non-repo dirs.
HARNESS_FLAGS = {
    "codex": ["codex", "exec", "--skip-git-repo-check"],
    "gemini": ["gemini", "--skip-trust", "--approval-mode", "plan", "-p"],
    "opencode": ["opencode", "run"],
}

# Environment variables per harness — only set if the key is actually present
# (gemini on Mac uses OAuth, not an API key; setting an empty key would break it)
HARNESS_ENV = {
    "codex": {},
    "gemini": {k: v for k, v in {"GEMINI_API_KEY": os.environ.get("GEMINI_API_KEY", "")}.items() if v},
    "opencode": {},
}

# Prompt templates
LOCALIZATION_SUFFIX = (
    "\n\nThis is a LOCALIZATION task only. Do NOT edit, create, or delete any files. "
    "Explore the codebase to understand the structure, then name the files you would "
    "change to accomplish this task. At the very end of your response, list the file "
    "paths in a section titled exactly:\n"
    "FILES TO CHANGE:\n"
    "<one file path per line>"
)

ARM_B_PREFIX = (
    'First run `gitpixel targets "{task}" .`. '
    "Work only from the returned P0/P1 list. "
    "Do not `ls`, `grep`, or read files outside it.\n\n"
)

# Arm C — soft closure: targets as starting point, bounded exploration allowed
ARM_C_PREFIX = (
    'First run `gitpixel targets "{task}" .`. '
    "Start your exploration from the returned P0/P1 list — read those files first. "
    "After reading them, if you believe files outside the list are needed, you may "
    "explore further with `ls`, `grep`, or file reads — but state why before each "
    "additional search outside the list.\n\n"
)

# Timeout per run (seconds)
RUN_TIMEOUT = 600


def load_tasks():
    with open(TASKS_FILE) as f:
        data = json.load(f)
    return data["tasks"]


def get_parent_commit(repo, commit):
    """Get the parent commit hash."""
    result = subprocess.run(
        ["git", "rev-parse", f"{commit}^"],
        cwd=str(repo), capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"Could not get parent of {commit}: {result.stderr}")
    return result.stdout.strip()


def checkout_commit(repo, commit):
    """Checkout a specific commit in the repo (detached HEAD)."""
    # Clean any uncommitted changes first
    subprocess.run(["git", "checkout", "--", "."], cwd=str(repo), capture_output=True)
    subprocess.run(["git", "clean", "-fd"], cwd=str(repo), capture_output=True)
    # Remove any .gitpixel index from previous runs
    gp_dir = repo / ".gitpixel"
    if gp_dir.exists():
        shutil.rmtree(gp_dir)
    result = subprocess.run(
        ["git", "checkout", "--detach", commit],
        cwd=str(repo), capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"Could not checkout {commit}: {result.stderr}")
    return result.stdout.strip()


def reset_repo(repo):
    """Reset repo to main branch and clean up."""
    subprocess.run(["git", "checkout", "--", "."], cwd=str(repo), capture_output=True)
    subprocess.run(["git", "clean", "-fd"], cwd=str(repo), capture_output=True)
    subprocess.run(["git", "checkout", "main"], cwd=str(repo), capture_output=True)
    gp_dir = repo / ".gitpixel"
    if gp_dir.exists():
        shutil.rmtree(gp_dir)


def build_index_graph(repo_path, gitpixel_bin):
    """Run gitpixel index + graph. Returns (success, elapsed_seconds, output)."""
    start = time.time()
    output_lines = []
    for cmd in ["index", "graph"]:
        result = subprocess.run(
            [str(gitpixel_bin), cmd, "."],
            cwd=str(repo_path),
            capture_output=True, text=True, timeout=120
        )
        output_lines.append(f"$ gitpixel {cmd} .")
        output_lines.append(result.stdout.strip())
        if result.stderr.strip():
            output_lines.append(result.stderr.strip())
        if result.returncode != 0:
            elapsed = time.time() - start
            return False, elapsed, "\n".join(output_lines) + f"\nFAILED: {cmd}"
    elapsed = time.time() - start
    return True, elapsed, "\n".join(output_lines)


def build_prompt(task, arm):
    """Build the prompt for the given task and arm."""
    task_text = task["task"]
    if arm == "A":
        prompt = task_text + LOCALIZATION_SUFFIX
    elif arm == "B":
        prefix = ARM_B_PREFIX.format(task=task_text)
        prompt = prefix + task_text + LOCALIZATION_SUFFIX
    elif arm == "C":
        prefix = ARM_C_PREFIX.format(task=task_text)
        prompt = prefix + task_text + LOCALIZATION_SUFFIX
    else:
        raise ValueError(f"Unknown arm: {arm}")
    return prompt


def run_harness(harness, prompt, repo_path, shim_dir, shim_log_path):
    """Run a single harness. Returns (returncode, stdout, stderr, elapsed)."""
    # Build environment
    env = os.environ.copy()
    # Prepend shim dir to PATH
    env["PATH"] = str(shim_dir) + os.pathsep + env.get("PATH", "")
    env["SHIM_LOG"] = str(shim_log_path)
    # Add harness-specific env vars
    env.update(HARNESS_ENV.get(harness, {}))

    # Build command
    flags = HARNESS_FLAGS[harness]
    cmd = flags + [prompt]

    start = time.time()
    try:
        result = subprocess.run(
            cmd,
            cwd=str(repo_path),
            capture_output=True,
            text=True,
            timeout=RUN_TIMEOUT,
            env=env,
        )
        elapsed = time.time() - start
        return result.returncode, result.stdout, result.stderr, elapsed
    except subprocess.TimeoutExpired:
        elapsed = time.time() - start
        return -1, "", f"TIMEOUT after {RUN_TIMEOUT}s", elapsed
    except Exception as e:
        elapsed = time.time() - start
        return -2, "", str(e), elapsed


def check_for_edits(repo_path):
    """Check if any files were modified in the repo."""
    result = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=str(repo_path), capture_output=True, text=True
    )
    return result.stdout.strip()


def reset_edits(repo_path):
    """Reset any file edits in the repo."""
    subprocess.run(
        ["git", "checkout", "--", "."],
        cwd=str(repo_path), capture_output=True
    )
    subprocess.run(
        ["git", "clean", "-fd"],
        cwd=str(repo_path), capture_output=True
    )


def run_single(task, arm, harness, run_id, artifacts_dir, repo_path, gitpixel_bin,
               index_built, index_build_seconds):
    """Run a single experiment cell. Returns a result dict."""
    result = {
        "run_id": run_id,
        "task_id": task["id"],
        "arm": arm,
        "harness": harness,
        "commit": task["commit"],
        "parent": task.get("_parent", ""),
        "status": "pending",
        "drop_reason": None,
        "wall_seconds": None,
        "index_build_seconds": index_build_seconds if arm in ("B", "C") else None,
        "shim_log_path": None,
        "transcript_path": None,
        "edits_detected": False,
    }

    # Create artifact dirs
    run_dir = artifacts_dir / run_id
    run_dir.mkdir(parents=True, exist_ok=True)
    shim_log_path = run_dir / "shim.log"
    transcript_path = run_dir / "transcript.txt"
    meta_path = run_dir / "meta.json"

    # Remove stale shim log
    if shim_log_path.exists():
        shim_log_path.unlink()

    try:
        # Build prompt
        prompt = build_prompt(task, arm)
        with open(run_dir / "prompt.txt", "w") as f:
            f.write(prompt)

        # Run the harness
        rc, stdout, stderr, elapsed = run_harness(
            harness, prompt, repo_path, SHIM_DIR, shim_log_path
        )

        result["wall_seconds"] = round(elapsed, 2)
        result["returncode"] = rc

        # Save transcript
        with open(transcript_path, "w") as f:
            f.write(f"=== STDOUT ===\n{stdout}\n\n=== STDERR ===\n{stderr}\n")

        result["shim_log_path"] = str(shim_log_path)
        result["transcript_path"] = str(transcript_path)

        # Check for crashes/timeouts
        if rc == -1:
            result["status"] = "dropped"
            result["drop_reason"] = "timeout"
        elif rc == -2:
            result["status"] = "dropped"
            result["drop_reason"] = f"exception: {stderr}"
        elif rc != 0:
            # Non-zero exit but we still got output — check if it's useful
            if not stdout.strip():
                result["status"] = "dropped"
                result["drop_reason"] = f"non-zero exit ({rc}), no stdout. stderr: {stderr[:300]}"
            else:
                result["status"] = "completed_with_error"
                result["drop_reason"] = f"non-zero exit ({rc}): {stderr[:300]}"
        else:
            result["status"] = "completed"

        # Check for file edits (localization-only violation)
        edits = check_for_edits(repo_path)
        if edits:
            result["edits_detected"] = True
            result["edit_details"] = edits
            with open(run_dir / "edits.txt", "w") as f:
                f.write(edits)
            reset_edits(repo_path)
            if result["status"] == "completed":
                result["status"] = "completed_edits_discarded"

    except Exception as e:
        result["status"] = "dropped"
        result["drop_reason"] = f"runner exception: {str(e)}"

    # Save metadata
    with open(meta_path, "w") as f:
        json.dump(result, f, indent=2)

    return result


def main():
    parser = argparse.ArgumentParser(description="Run sniper-discovery experiment")
    parser.add_argument("--warmup", action="store_true", help="Run 3 warmup runs (discarded)")
    parser.add_argument("--max-concurrent", type=int, default=4, help="Max concurrent agents per task")
    parser.add_argument("--harnesses", type=str, default="codex,gemini,opencode",
                        help="Comma-separated harness list")
    parser.add_argument("--tasks", type=str, default=None,
                        help="Comma-separated task IDs (default: all)")
    parser.add_argument("--arms", type=str, default="A,B",
                        help="Comma-separated arms (default: A,B)")
    args = parser.parse_args()

    tasks = load_tasks()
    harnesses = args.harnesses.split(",")
    arms = args.arms.split(",")

    if args.tasks:
        task_ids = set(args.tasks.split(","))
        tasks = [t for t in tasks if t["id"] in task_ids]

    # Ensure shim dir exists
    if not SHIM_DIR.exists():
        print("Generating shim wrappers...", file=sys.stderr)
        subprocess.run([
            sys.executable, str(SCRIPT_DIR / "gen_shim.py"),
            str(SHIM_DIR), str(GITPIXEL_BIN)
        ], check=True)

    # Ensure artifacts dir exists (gitignored)
    ARTIFACTS_DIR.mkdir(parents=True, exist_ok=True)

    all_results = []

    if args.warmup:
        print("=== WARMUP RUNS (discarded) ===", file=sys.stderr)
        warmup_dir = ARTIFACTS_DIR / "warmup"
        warmup_dir.mkdir(parents=True, exist_ok=True)
        warmup_task = tasks[0]
        parent = get_parent_commit(TEST_REPO, warmup_task["commit"])
        warmup_task["_parent"] = parent
        checkout_commit(TEST_REPO, parent)
        for i, h in enumerate(harnesses[:3]):
            run_id = f"warmup-{i+1}"
            print(f"  Warmup {run_id}: {warmup_task['id']} arm=A harness={h}", file=sys.stderr)
            result = run_single(warmup_task, "A", h, run_id, warmup_dir, TEST_REPO,
                                GITPIXEL_BIN, False, None)
            reset_edits(TEST_REPO)
            print(f"    -> {result['status']} ({result.get('wall_seconds', '?')}s)", file=sys.stderr)
        reset_repo(TEST_REPO)
        print("Warmup complete.\n", file=sys.stderr)

    # Main runs: tasks sequential, cells within task parallel (max 4)
    total_cells = len(tasks) * len(harnesses) * len(arms)
    print(f"=== MAIN RUNS: {len(tasks)} tasks × {len(harnesses)} harnesses × {len(arms)} arms "
          f"= {total_cells} cells, max {args.max_concurrent} concurrent per task ===",
          file=sys.stderr)

    completed = 0
    for task in tasks:
        task_id = task["id"]
        parent = get_parent_commit(TEST_REPO, task["commit"])
        task["_parent"] = parent

        print(f"\n--- Task {task_id} (checkout {parent[:8]}) ---", file=sys.stderr)

        # Checkout parent commit
        try:
            checkout_commit(TEST_REPO, parent)
        except Exception as e:
            print(f"  FAILED to checkout: {e}", file=sys.stderr)
            for arm in arms:
                for h in harnesses:
                    run_id = f"{task_id}-{arm}-{h}"
                    all_results.append({
                        "run_id": run_id, "task_id": task_id, "arm": arm,
                        "harness": h, "status": "dropped",
                        "drop_reason": f"checkout failed: {e}",
                    })
                    completed += 1
            continue

        # Build index + graph for arms that need it (B and C both run gitpixel targets)
        index_built = False
        index_build_seconds = None
        idx_log = ""
        if "B" in arms or "C" in arms:
            print(f"  Building index + graph for sniper arms...", file=sys.stderr)
            ok, idx_secs, idx_out = build_index_graph(TEST_REPO, GITPIXEL_BIN)
            index_build_seconds = idx_secs
            idx_log = idx_out
            if ok:
                index_built = True
                print(f"    Index built in {idx_secs:.1f}s", file=sys.stderr)
            else:
                print(f"    Index build FAILED in {idx_secs:.1f}s", file=sys.stderr)

        # Save index build log
        if idx_log:
            idx_dir = ARTIFACTS_DIR / f"{task_id}-index-build"
            idx_dir.mkdir(parents=True, exist_ok=True)
            with open(idx_dir / "log.txt", "w") as f:
                f.write(idx_log)

        # Build the cell list for this task
        cells = []
        for arm in arms:
            for harness in harnesses:
                run_id = f"{task_id}-{arm}-{harness}"
                cells.append((arm, harness, run_id))

        # Run cells with max concurrency
        with ThreadPoolExecutor(max_workers=min(args.max_concurrent, len(cells))) as executor:
            futures = {}
            for arm, harness, run_id in cells:
                # Skip sniper arms if index build failed
                if arm in ("B", "C") and not index_built:
                    all_results.append({
                        "run_id": run_id, "task_id": task_id, "arm": arm,
                        "harness": harness, "status": "dropped",
                        "drop_reason": "index/graph build failed",
                        "index_build_seconds": index_build_seconds,
                    })
                    completed += 1
                    print(f"  [{completed}/{total_cells}] {run_id}: dropped (index build failed)",
                          file=sys.stderr)
                    continue

                future = executor.submit(
                    run_single, task, arm, harness, run_id, ARTIFACTS_DIR,
                    TEST_REPO, GITPIXEL_BIN, index_built, index_build_seconds
                )
                futures[future] = run_id

            for future in as_completed(futures):
                run_id = futures[future]
                completed += 1
                try:
                    result = future.result()
                    all_results.append(result)
                    status = result["status"]
                    wall = result.get("wall_seconds", "?")
                    drop = result.get("drop_reason", "")
                    print(f"  [{completed}/{total_cells}] {run_id}: {status} "
                          f"({wall}s){f' DROP: {drop}' if drop else ''}",
                          file=sys.stderr)
                    # Reset any edits between runs
                    reset_edits(TEST_REPO)
                except Exception as e:
                    print(f"  [{completed}/{total_cells}] {run_id}: EXCEPTION {e}",
                          file=sys.stderr)
                    all_results.append({
                        "run_id": run_id, "task_id": task_id,
                        "status": "dropped",
                        "drop_reason": f"executor exception: {str(e)}",
                    })

        # Reset repo after task
        reset_repo(TEST_REPO)

    # Save results summary
    results_path = ARTIFACTS_DIR / "results.json"
    with open(results_path, "w") as f:
        json.dump(all_results, f, indent=2)

    # Print summary
    print(f"\n=== SUMMARY ===", file=sys.stderr)
    statuses = {}
    for r in all_results:
        s = r["status"]
        statuses[s] = statuses.get(s, 0) + 1
    for s, c in sorted(statuses.items()):
        print(f"  {s}: {c}", file=sys.stderr)

    dropped = [r for r in all_results if r["status"].startswith("dropped")]
    if dropped:
        print(f"\nDropped runs ({len(dropped)}):", file=sys.stderr)
        for r in dropped:
            print(f"  {r['run_id']}: {r.get('drop_reason', '?')}", file=sys.stderr)

    print(f"\nResults saved to {results_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
