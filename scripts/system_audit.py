#!/usr/bin/env python3
"""Real CLI audit in disposable repositories/homes. Never uses a live remote/browser.

Build the candidate first; pass its absolute path. The report identifies that
binary and records exact argv, exit codes and assertions, not help-based coverage.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import subprocess
import tempfile
import time


class Audit:
    def __init__(self, pixel, root):
        self.pixel = pixel
        self.root = root
        self.repo = root / "repo"
        self.repo.mkdir()
        self.home = root / "home"
        self.home.mkdir()
        fake_bin = root / "fake-bin"
        fake_bin.mkdir()
        for name in ("claude", "agent-browser", "devin", "codex"):
            fake = fake_bin / name
            fake.write_text("#!/bin/sh\nprintf 'audit substitute: external executor unavailable\\n' >&2\nexit 127\n")
            fake.chmod(0o700)
        self.env = {
            **os.environ,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_DATA_HOME": str(self.home / "data"),
            "XDG_CACHE_HOME": str(self.home / "cache"),
            "PIXEL_DAEMON_AUTO_START": "0",
            "PIXEL_METRICS": "0",
            "PIXEL_FLOW_DIR": str(self.home / "flows"),
            "PATH": str(fake_bin) + os.pathsep + os.environ.get("PATH", ""),
            "PIXEL_CLAUDE_EXECUTABLE": str(fake_bin / "claude"),
            "PIXEL_TASK_BOUNDARY": "0",
            "PIXEL_TASK_CONTEXT": "1",
            "PIXEL_POST_COMPACTION": "1",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Audit fixture",
            "GIT_AUTHOR_EMAIL": "fixture@example.test",
            "GIT_COMMITTER_NAME": "Audit fixture",
            "GIT_COMMITTER_EMAIL": "fixture@example.test",
        }
        for key in ("ANTHROPIC_API_KEY", "PIXEL_RECALL_HOME", "PIXEL_HOME", "GIT_DIR", "GIT_WORK_TREE"):
            self.env.pop(key, None)
        self.results = []

    def git(self, *args):
        return subprocess.check_output(["git", *args], cwd=self.repo, env=self.env, text=True, stderr=subprocess.PIPE).strip()

    def call(self, leaf, args, *, contains=None, json_output=False, exit_code=0, input_text=None):
        started = time.monotonic()
        output = subprocess.run([str(self.pixel), *args], cwd=self.repo, env=self.env,
                                input=input_text, capture_output=True, text=True, timeout=30)
        error = None
        try:
            assert output.returncode == exit_code, f"expected exit {exit_code}, got {output.returncode}: {output.stderr}"
            if contains is not None:
                assert contains in output.stdout, f"missing {contains!r}: {output.stdout[:1200]}"
            if json_output == "ndjson":
                assert output.stdout.strip(), "expected at least one log record"
                for line in output.stdout.splitlines():
                    json.loads(line)
            elif json_output:
                json.loads(output.stdout)
        except (AssertionError, json.JSONDecodeError) as failure:
            error = str(failure)
        row = {"leaf": leaf, "argv": args, "exit_code": output.returncode,
               "seconds": round(time.monotonic() - started, 4),
               "assertion": {"contains": contains, "json": json_output, "exit": exit_code},
               "status": "FAIL" if error else "PASS", "error": error}
        self.results.append(row)
        print(json.dumps(row), flush=True)
        return output.stdout

    def fixture(self):
        self.git("init", "-b", "main")
        (self.repo / "lib.rs").write_text('pub fn login_user(name: &str) -> bool { !name.is_empty() }\npub fn main_entry() { login_user("fixture"); }\n')
        (self.repo / "removed.md").write_text("removed_manual_history evidence\n")
        (self.repo / ".gitignore").write_text(".pixel/\n.env\n")
        self.git("add", ".")
        self.git("commit", "-m", "fixture: original login and manual")
        self.base = self.git("rev-parse", "HEAD")
        self.git("mv", "removed.md", "manual.md")
        self.git("commit", "-m", "fixture: rename manual")
        self.git("rm", "manual.md")
        self.git("commit", "-m", "fixture: delete manual")
        self.tip = self.git("rev-parse", "HEAD")

    def retrieval(self):
        self.call("index", ["index", "--history"])
        assert (self.repo / ".pixel").is_dir(), "index did not create repository state"
        self.call("graph", ["graph", "--json"], contains="symbols", json_output=True)
        self.call("status", ["status", "--json"], json_output=True)
        self.call("stats", ["stats"], contains="files")
        self.call("search", ["search", "login_user", "--no-daemon"], contains="login_user")
        self.call("search-compat", ["search-compat", "grep", "--", "-n", "login_user", "lib.rs"], contains="login_user")
        self.call("query", ["query", "login_user", "--no-daemon", "--json"], contains="login_user", json_output=True)
        self.call("symbol", ["symbol", "login_user", "--json"], contains="login_user", json_output=True)
        self.call("skeleton", ["skeleton", "lib.rs", "--json"], contains="login_user", json_output=True)
        self.call("map", ["map", "--json"], contains="login_user", json_output=True)
        self.call("context", ["context", "lib.rs#login_user#function", "--budget", "300", "--json"], contains="login_user", json_output=True)
        self.call("targets", ["targets", "fix login_user", "--json", "--no-manifest"], contains="lib.rs", json_output=True)
        self.call("resolve", ["resolve", "login user", "--json"], contains="login_user", json_output=True)
        for leaf, args in [
            ("impact", ["impact", "login_user", "--json"]),
            ("uses", ["uses", "login_user", "--role", "callers", "--json"]),
            ("trace", ["trace", "main_entry", "login_user", "--json"]),
            ("processes", ["processes", "--json"]),
            ("clusters", ["clusters", "--json"]),
        ]:
            self.call(leaf, args, json_output=True)
        for leaf, args in [
            ("note set", ["note", "set", "lib.rs", "login_user", "audit note", "--json"]),
            ("note get", ["note", "get", "lib.rs", "login_user", "--json"]),
            ("note list", ["note", "list", "lib.rs", "--json"]),
            ("note rm", ["note", "rm", "lib.rs", "login_user", "--json"]),
        ]:
            self.call(leaf, args, json_output=True)
        with (self.repo / "lib.rs").open("a") as source:
            source.write('pub fn changed_function() {}\n')
        self.call("changes", ["changes", "--json"], contains="lib.rs", json_output=True)
        self.call("inspect", ["inspect", "--json"], contains="lib.rs", json_output=True)
        self.call("review", ["review", "--json"], contains="lib.rs", json_output=True)
        self.call("diff", ["diff", "HEAD", "--json"], contains="changed_function", json_output=True)
        self.call("history", ["history", "--json"], contains="fixture", json_output=True)
        self.call("history-search", ["history-search", "removed_manual_history", "--json"], contains="removed_manual_history", json_output=True)
        self.call("lifecycle", ["lifecycle", "--file", "removed.md", "--json"], contains="removed.md", json_output=True)
        self.call("excavate", ["excavate", "--phrase", "removed_manual_history", "--json"], contains="removed_manual_history", json_output=True)
        self.call("rescue", ["rescue", "login", "--file", "lib.rs", "--json"], contains="lib.rs", json_output=True)
        self.call("provenance", ["provenance", "lib.rs", "--json"], contains="fixture", json_output=True)
        self.call("branches", ["branches", "--json"], contains="main", json_output=True)
        self.call("journal", ["journal", "read", "--file", "lib.rs", "--detail", "audit fixture", "--json"], json_output=True)
        self.call("log", ["log", "--json"], json_output="ndjson")
        self.call("savings", ["savings", "--json"], json_output=True)

    def mutations(self):
        remote = self.root / "remote.git"
        self.git("init", "--bare", str(remote))
        self.git("remote", "add", "origin", str(remote))
        self.git("push", "-u", "origin", "main")
        self.call("branch", ["branch", "audit", "--request-id", "audit-branch", "--json"], contains="audit", json_output=True)
        self.git("switch", "audit")
        self.call("publish", ["publish", "--message", "fixture: preserve changed function", "--files", "lib.rs", "--request-id", "audit-publish", "--json"], json_output=True)
        published = self.git("rev-parse", "HEAD")
        assert published != self.tip
        self.call("publish", ["publish", "--message", "fixture: preserve changed function", "--files", "lib.rs", "--request-id", "audit-publish", "--json"], json_output=True)
        assert self.git("rev-parse", "HEAD") == published, "idempotent publish made a second commit"
        self.call("push", ["push", "origin", "HEAD:refs/heads/audit", "--request-id", "audit-push", "--json"], json_output=True)
        assert self.git("--git-dir", str(remote), "rev-parse", "refs/heads/audit") == published
        with (self.repo / "lib.rs").open("a") as source:
            source.write("pub fn shipped_function() {}\n")
        self.call("ship", ["ship", "origin", "HEAD:refs/heads/audit", "--message", "fixture: ship second function", "--files", "lib.rs", "--request-id", "audit-ship", "--json"], json_output=True)
        shipped = self.git("rev-parse", "HEAD")
        assert shipped != published
        assert self.git("--git-dir", str(remote), "rev-parse", "refs/heads/audit") == shipped
        self.call("sync", ["sync", "origin", "--json"], json_output=True)
        self.call("reconcile", ["reconcile", "--strategy", "report", "--push", "none", "--json"], json_output=True)
        self.git("switch", "-c", "audit-update", self.tip)
        self.call("update", ["update", "--expected-head", self.tip, "--target-oid", shipped, "--request-id", "audit-update", "--json"], json_output=True)
        assert self.git("rev-parse", "HEAD") == shipped
        before = (self.repo / "lib.rs").read_bytes()
        self.call("rewrite", ["rewrite", "--onto", self.tip, "--expected-head", shipped, "--request-id", "audit-rewrite", "--json"], json_output=True)
        assert (self.repo / "lib.rs").read_bytes() == before
        assert self.git("rev-list", "--count", f"{self.tip}..HEAD") == "1"

    def environment(self):
        env_file = self.repo / ".env"
        original = b"# preserved fixture\nUNRELATED=fixture_secret_not_for_output\nEXISTING=before\n"
        env_file.write_bytes(original)
        for leaf, args in [
            ("env inventory", ["env", "inventory", "--json"]),
            ("env set", ["env", "set", "--file", ".env", "--key", "EXISTING", "--value", "after", "--json"]),
            ("env check", ["env", "check", "--file", ".env", "--require", "UNRELATED", "--json"]),
            ("env snapshots", ["env", "snapshots", "--file", ".env", "--json"]),
        ]:
            output = self.call(leaf, args, json_output=True)
            assert "fixture_secret_not_for_output" not in output, "environment value leaked"
        assert env_file.read_bytes() == original.replace(b"EXISTING=before", b"EXISTING=after")
        self.call("env restore", ["env", "restore", "--file", ".env", "--json"], json_output=True)
        assert env_file.read_bytes() == original, "snapshot did not restore exact bytes"

    def sniper(self):
        error = {"surface": "reported", "message": "audit_error real boundary", "run_id": "audit-run"}
        recorded = json.loads(self.call("sniper report", ["sniper", "report", "--json"], json_output=True, input_text=json.dumps(error)))
        error_id = str(recorded["id"])
        for event in [
            {"type": "run", "run_id": "audit-run", "pid": 123, "port": 4321},
            {"type": "event", "kind": "hmr-update", "run_id": "audit-run", "data": {"files": ["lib.rs"]}},
            {"type": "event", "kind": "test-pass", "run_id": "audit-run"},
        ]:
            self.call("sniper report", ["sniper", "report", "--json"], json_output=True, input_text=json.dumps(event))
        for leaf, args, text in [
            ("sniper last", ["sniper", "last", "--json"], "audit_error"),
            ("sniper since", ["sniper", "since", "0", "--json"], "audit_error"),
            ("sniper show", ["sniper", "show", error_id, "--json"], "audit_error"),
            ("sniper query", ["sniper", "query", "audit_error", "--json"], "audit_error"),
            ("sniper hmr", ["sniper", "hmr", "--json"], "lib.rs"),
            ("sniper env", ["sniper", "env", "--json"], "audit-run"),
            ("sniper test", ["sniper", "test", "--json"], "test-pass"),
            ("sniper cursor", ["sniper", "cursor", "--json"], error_id),
            ("sniper gc", ["sniper", "gc", "--json"], None),
        ]:
            self.call(leaf, args, contains=text, json_output=True)
        self.call("sniper run", ["sniper", "run", "--", "/bin/sh", "-c", "printf audit_wrapper_failure >&2; exit 9"], exit_code=9)
        # Search indexes the error message; the captured tail is separate extra data.
        self.call("sniper query", ["sniper", "query", "exited 9", "--json"], contains="audit_wrapper_failure", json_output=True)
        self.mcp(int(error_id))

    def mcp(self, error_id):
        argv = [str(self.pixel), "sniper", "mcp"]
        process = subprocess.Popen(argv, cwd=self.repo, env=self.env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, text=True)
        def exchange(payload):
            process.stdin.write(json.dumps(payload) + "\n")
            process.stdin.flush()
            assert select.select([process.stdout], [], [], 15)[0], "MCP response timeout"
            response = json.loads(process.stdout.readline())
            assert response.get("id") == payload["id"], response
            assert "error" not in response, response
            return response
        started = time.monotonic()
        try:
            exchange({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "audit-fixture", "version": "1"}}})
            process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            process.stdin.flush()
            listing = exchange({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
            assert len(listing["result"]["tools"]) == 5
            for request_id, (name, arguments, expected) in enumerate([
                ("errors_since", {"cursor": 0}, "audit_error"),
                ("error_show", {"id": error_id}, "audit_error"),
                ("errors_query", {"text": "audit_error"}, "audit_error"),
                ("hmr_status", {"file": "lib.rs"}, "lib.rs"),
                ("env_fingerprint", {"diff": False}, "run_id"),
            ], 3):
                result = exchange({"jsonrpc": "2.0", "id": request_id, "method": "tools/call", "params": {"name": name, "arguments": arguments}})
                assert expected in json.dumps(result), result
            process.stdin.close()
            assert process.wait(timeout=10) == 0
            self.results.append({"leaf": "sniper mcp", "argv": ["sniper", "mcp"], "exit_code": 0, "status": "PASS", "seconds": round(time.monotonic() - started, 4), "assertion": "initialize, list exactly 5 tools, call all 5 against populated store, EOF cleanup"})
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    def tasks(self):
        task = json.loads(self.call("task begin", ["task", "begin", "fix login_user boundary", "--session", "audit-session", "--provider", "codex", "--json"], json_output=True))["task_id"]
        task = json.loads(self.call("task accept", ["task", "accept", "fix login_user boundary", "--session", "accepted-session", "--provider", "claude", "--json"], contains="accepted", json_output=True))["task_id"]
        for leaf in ["prepare", "status", "events"]:
            self.call(f"task {leaf}", ["task", leaf, task, "--json"], contains=task, json_output=True)
        plan = self.root / "plan.json"
        plan.write_text(json.dumps({"lanes": [{"id": "login-lane", "owned_paths": ["lib.rs"], "symbols": ["lib.rs#login_user#function"], "depends_on": [], "candidate_count": 1}]}))
        self.call("task plan-validate", ["task", "plan-validate", task, "--file", str(plan), "--json"], json_output=True)
        candidate = json.loads(self.call("task sandbox-create", ["task", "sandbox-create", task, "candidate", "--owned-path", "lib.rs", "--json"], contains="sandbox_root", json_output=True))
        sandbox = Path(candidate["sandbox_root"])
        with (sandbox / "lib.rs").open("a") as source:
            source.write("pub fn sandbox_verified() {}\n")
        self.call("task sandbox-inspect", ["task", "sandbox-inspect", task, "candidate", "--json"], contains="eligible", json_output=True)
        self.call("task sandbox-promote", ["task", "sandbox-promote", task, "candidate", "--json"], contains="promoted", json_output=True)
        assert "sandbox_verified" in (self.repo / "lib.rs").read_text()
        self.call("task sandbox-cleanup", ["task", "sandbox-cleanup", task, "candidate", "--json"], json_output=True)
        assert not sandbox.exists()
        self.git("add", "lib.rs")
        self.git("commit", "-m", "fixture: promoted sandbox")
        worker = self.root / "fake-worker"
        worker.write_text("#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do /bin/sleep 1; done\n")
        worker.chmod(0o700)
        cleanup = [(task, "worker")]
        try:
            self.call("task sandbox-create", ["task", "sandbox-create", task, "worker", "--owned-path", "lib.rs", "--json"], json_output=True)
            self.call("task worker-start", ["task", "worker-start", task, "worker", "--executable", str(worker), "--json"], json_output=True)
            self.call("task worker-status", ["task", "worker-status", task, "worker", "--json"], contains="running", json_output=True)
            self.call("task worker-stop", ["task", "worker-stop", task, "worker", "--json"], json_output=True)
            self.call("task sandbox-cancel", ["task", "sandbox-cancel", task, "worker", "--json"], json_output=True)
            # A completed/stopped worker task is not a new race authorization.
            task = json.loads(self.call("task accept", ["task", "accept", "race login implementations", "--provider", "claude", "--session", "race-session", "--json"], contains="accepted", json_output=True))["task_id"]
            for candidate_id in ["winner", "loser"]:
                cleanup.append((task, candidate_id))
                self.call("task sandbox-create", ["task", "sandbox-create", task, candidate_id, "--owned-path", "lib.rs", "--json"], json_output=True)
            worker.write_text("#!/bin/sh\nif [ \"$PIXEL_WORKTREE_ID\" = winner ]; then printf '\\npub fn race_winner() {}\\n' >> lib.rs; git add lib.rs; exit 0; fi\ntrap 'exit 0' TERM\nwhile :; do /bin/sleep 1; done\n")
            self.call("task race-start", ["task", "race-start", task, "winner", "loser", "--executable", str(worker), "--json"], json_output=True)
            assert self.results[-1]["status"] == "PASS", "race did not start"
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                self.call("task race-poll", ["task", "race-poll", task, "winner", "loser", "--json"], json_output=True)
                if "race_winner" in (self.repo / "lib.rs").read_text():
                    break
                time.sleep(0.1)
            assert "race_winner" in (self.repo / "lib.rs").read_text(), "race winner not promoted"
        finally:
            for task_id, candidate_id in cleanup:
                subprocess.run([str(self.pixel), "task", "worker-stop", task_id, candidate_id, "--json"], cwd=self.repo, env=self.env, capture_output=True, timeout=10)
                subprocess.run([str(self.pixel), "task", "sandbox-cleanup", task_id, candidate_id, "--json"], cwd=self.repo, env=self.env, capture_output=True, timeout=10)
        self.call("task show", ["task", "show", "--session", "audit-session", "--json"], json_output=True)
        self.call("task reset", ["task", "reset", "--session", "audit-session", "--json"], json_output=True)

    def hooks(self):
        # Prior mutation fixtures rewrote HEAD; refresh history before saving a
        # current-head manifest. Post-compaction correctly refuses stale hints.
        self.call("index", ["index", "--history"])
        self.call("ready", ["ready", "--json"], json_output=True)
        self.call("targets", ["targets", "fix login_user", "--json"], contains="lib.rs", json_output=True)
        payload = {"cwd": str(self.repo), "session_id": "hook-audit", "hook_event_name": "PreToolUse", "tool_name": "shell", "tool_input": {"command": "grep -n login_user lib.rs"}}
        self.call("hook guard", ["hook", "guard", "--provider", "codex"], input_text=json.dumps(payload), contains="pixel search-compat", json_output=True)
        foreign = self.root / "foreign-hook.sh"
        foreign.write_text("printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"additionalContext\":\"audit foreign context\"}}'\n")
        backup = self.root / "foreign-hook.json"
        backup.write_text(json.dumps({"version": 1, "provider": "codex", "pre_tool_use": [{"matcher": "shell", "hooks": [{"type": "command", "command": f'/bin/sh "{foreign}"'}]}], "managed_pre_tool_use": []}))
        backup.chmod(0o600)
        self.call("hook composed-guard", ["hook", "composed-guard", "--backup", str(backup)], input_text=json.dumps(payload), contains="audit foreign context", json_output=True)
        self.call("hook session-start", ["hook", "session-start"], input_text="{}", contains="capabilities", json_output=True)
        # This hook only queries an already-running daemon; warm the disposable one.
        try:
            self.call("hook prompt-submit", ["hook", "prompt-submit", "--provider", "codex"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "hook-audit", "prompt": "Fix login_user validation", "hook_event_name": "UserPromptSubmit"}), contains="lib.rs", json_output=True)
            # A question creates a real session packet without an automatic coding handoff.
            self.call("hook prompt-submit", ["hook", "prompt-submit", "--provider", "claude"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "display-session", "prompt": "How does login_user validation work?", "hook_event_name": "UserPromptSubmit"}), contains="lib.rs", json_output=True)
            self.call("task show", ["task", "show", "--session", "display-session", "--json"], contains="display-session", json_output=True)
            self.call("task reset", ["task", "reset", "--session", "display-session", "--json"], json_output=True)
            reset = self.call("task show", ["task", "show", "--session", "display-session", "--json"], json_output=True)
            assert json.loads(reset)["status"] == "absent" and json.loads(reset)["task_id"] is None, "reset retained the populated session packet"
        finally:
            self.call("daemon stop", ["daemon", "stop"])
        self.call("hook post-compaction", ["hook", "post-compaction"], input_text=json.dumps({"cwd": str(self.repo), "session_id": "hook-audit", "hook_event_name": "PostCompaction"}), contains="lib.rs", json_output=True)
        self.call("hook post-tool-use", ["hook", "post-tool-use", "--provider", "claude"], input_text=json.dumps({"cwd": str(self.repo), "tool_name": "Edit", "tool_input": {"file_path": str(self.repo / "lib.rs")}}), contains="PostToolUse", json_output=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pixel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    pixel = args.pixel.resolve(strict=True)
    report = {"candidate": str(pixel)}
    with tempfile.TemporaryDirectory(prefix="pixel-system-audit-") as temporary:
        # Freeze the executable so concurrent rebuilds cannot mix candidate versions.
        frozen = Path(temporary) / "pixel-candidate"
        shutil.copy2(pixel, frozen)
        report["sha256"] = hashlib.sha256(frozen.read_bytes()).hexdigest()
        audit = Audit(frozen, Path(temporary))
        try:
            audit.fixture()
            audit.retrieval()
            audit.mutations()
            audit.environment()
            audit.sniper()
            audit.hooks()
            audit.tasks()
        except Exception as error:
            audit.results.append({"leaf": "integration invariant", "status": "FAIL", "error": str(error)})
            raise
        finally:
            report["results"] = audit.results
            report["passed"] = sum(row["status"] == "PASS" for row in audit.results)
            report["failed"] = sum(row["status"] == "FAIL" for row in audit.results)
            args.output.write_text(json.dumps(report, indent=2) + "\n")
    if report["failed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
