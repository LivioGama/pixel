#!/usr/bin/env python3
"""Contract of scripts/gates.sh: skip-if-untouched, fail-open, laptop-safe env.

Runs the real script inside a disposable git repository with a stub `cargo`
on PATH that records every invocation and the environment it saw.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

GATES = Path(__file__).with_name("gates.sh")


class GatesContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-gates-contract-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        (self.repo / "scripts").mkdir(parents=True)
        shutil.copy(GATES, self.repo / "scripts/gates.sh")
        (self.repo / "src").mkdir()
        (self.repo / "src/lib.rs").write_text("pub fn a() {}\n")
        (self.repo / "README.md").write_text("readme\n")
        self.git("init", "-q", "-b", "develop")
        self.git("add", ".")
        self.git("commit", "-qm", "base")
        self.git("switch", "-q", "-c", "feature")
        fake = self.root / "fake"
        fake.mkdir()
        self.log = self.root / "cargo.log"
        # `cargo nextest --version` answers only when HAVE_NEXTEST=1, so both
        # test-runner branches of the script are exercised.
        (fake / "cargo").write_text(
            "#!/bin/sh\n"
            'if [ "$1" = nextest ] && [ "$2" = --version ]; then [ "${HAVE_NEXTEST:-0}" = 1 ]; exit $?; fi\n'
            'echo "cargo $1 jobs=${CARGO_BUILD_JOBS:-unset} threads=${RUST_TEST_THREADS:-unset}" >> "$GATES_LOG"\n'
            'case "$1" in clippy) exit "${FAIL_CLIPPY:-0}";; esac\n'
        )
        (fake / "cargo").chmod(0o755)
        self.env = {
            k: v for k, v in os.environ.items()
            if k not in ("CI", "CARGO_BUILD_JOBS", "RUST_TEST_THREADS")
        }
        self.env.update({
            "PATH": str(fake) + os.pathsep + os.environ["PATH"],
            "GATES_LOG": str(self.log),
            "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com",
        })

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.repo), *args], check=True,
                       capture_output=True, env={**os.environ,
                       "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.com",
                       "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.com"})

    def gates(self, *args, **extra_env):
        return subprocess.run(
            ["sh", str(self.repo / "scripts/gates.sh"), *args],
            cwd=self.repo, env={**self.env, **extra_env},
            capture_output=True, text=True, timeout=30,
        )

    def invocations(self):
        return self.log.read_text().splitlines() if self.log.exists() else []

    def test_docs_only_change_skips_every_cargo_invocation(self):
        (self.repo / "README.md").write_text("edited\n")
        self.git("commit", "-qam", "docs")
        (self.repo / "NOTES.md").write_text("untracked prose\n")
        result = self.gates()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping", result.stdout)
        self.assertEqual(self.invocations(), [])

    def test_rust_change_runs_the_three_gates_in_order_with_safe_defaults(self):
        (self.repo / "src/lib.rs").write_text("pub fn b() {}\n")
        self.git("commit", "-qam", "rust")
        result = self.gates()
        self.assertEqual(result.returncode, 0, result.stderr)
        # Two cores stay free for the desktop; test binaries get half the
        # CPUs (each integration test spawns a pixel process plus git).
        threads = max(2, (os.cpu_count() or 4) // 2)
        self.assertEqual(
            self.invocations(),
            [f"cargo fmt jobs=-2 threads={threads}",
             f"cargo clippy jobs=-2 threads={threads}",
             f"cargo test jobs=-2 threads={threads}"],
        )
        self.assertIn("all gates passed", result.stdout)

    def test_nextest_when_installed_then_doctests_which_nextest_skips(self):
        (self.repo / "src/lib.rs").write_text("pub fn b() {}\n")
        result = self.gates(HAVE_NEXTEST="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [line.split()[1] for line in self.invocations()],
            ["fmt", "clippy", "nextest", "test"],
        )
        self.assertIn("cargo test --doc ok", result.stdout)

    def test_dirty_cargo_manifest_counts_as_rust_affecting(self):
        (self.repo / "Cargo.toml").write_text("[package]\n")
        result = self.gates()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.invocations()), 3)

    def test_explicit_environment_values_win_over_the_defaults(self):
        (self.repo / "src/lib.rs").write_text("pub fn b() {}\n")
        result = self.gates(CARGO_BUILD_JOBS="3", RUST_TEST_THREADS="5")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.invocations()[0], "cargo fmt jobs=3 threads=5")

    def test_force_runs_the_gates_without_a_rust_change(self):
        result = self.gates("--force")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.invocations()), 3)

    def test_ci_never_skips_and_leaves_the_job_count_to_the_runner(self):
        result = self.gates(CI="true")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.invocations(),
            ["cargo fmt jobs=unset threads=unset",
             "cargo clippy jobs=unset threads=unset",
             "cargo test jobs=unset threads=unset"],
        )

    def test_a_red_gate_stops_the_run_and_propagates_its_exit_code(self):
        (self.repo / "src/lib.rs").write_text("pub fn b() {}\n")
        result = self.gates(FAIL_CLIPPY="7")
        self.assertEqual(result.returncode, 7)
        self.assertIn("cargo clippy FAILED", result.stderr)
        self.assertEqual([line.split()[1] for line in self.invocations()], ["fmt", "clippy"])

    def test_missing_develop_branch_fails_open(self):
        self.git("branch", "-D", "develop")
        result = self.gates()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(self.invocations()), 3)

    def test_unknown_argument_is_rejected(self):
        result = self.gates("--bogus")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.invocations(), [])


if __name__ == "__main__":
    unittest.main()
