#!/usr/bin/env python3
"""Contract of .cargo/mutants.toml: an exclusion only ever covers a bench or
a crate-root build script.

`exclude_globs` is the one place where a file can leave the mutation gate
without anyone noticing: cargo-mutants reports nothing for a path it was
told to skip, so the `Mutants` job stays green over code it never mutated.
The trap this file exists for: `**/build.rs` reads as "cargo build scripts"
but matches any source file of that name, and this workspace has one --
`crates/pixel-graph/src/build.rs`, the 2000-line module behind `build_graph`,
`tree_delta` and the freshness signature. It sat outside the gate for as
long as the glob did.

So the rule below is deliberately narrow: every Rust file an exclusion
covers must be a bench (not shipped, no tests of its own) or a build script
at a crate root (a separate compilation unit; a mutated one changes what the
build reports, not what a test asserts). Anything else is a module, and a
module belongs to the gate.
"""

import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
CONFIG = REPO / ".cargo/mutants.toml"
GATE = REPO / "scripts/mutants-gate.py"

#: A bench: the dedicated crate, or a `benches/` directory in any crate.
BENCH = re.compile(r"^crates/pixel-bench/|(^|/)benches/")
#: A cargo build script: `build.rs` directly at a crate root, never deeper.
BUILD_SCRIPT = re.compile(r"^crates/[^/]+/build\.rs$")


def exclude_globs() -> list[str]:
    """The `exclude_globs` array, read without a TOML dependency."""
    text = CONFIG.read_text()
    match = re.search(r"^exclude_globs\s*=\s*\[(.*?)\]", text, re.M | re.S)
    assert match, f"{CONFIG} declares no exclude_globs"
    return re.findall(r'"([^"]+)"', match.group(1))


def to_regex(glob: str) -> re.Pattern[str]:
    """Translate one glob to a regex with globset's semantics.

    The distinction this contract turns on: `*` stops at a path separator,
    `**` crosses them. Anything else is matched literally.
    """
    out = ["^"]
    i = 0
    while i < len(glob):
        if glob.startswith("**/", i):
            out.append("(?:.*/)?")
            i += 3
        elif glob.startswith("**", i):
            out.append(".*")
            i += 2
        elif glob[i] == "*":
            out.append("[^/]*")
            i += 1
        elif glob[i] == "?":
            out.append("[^/]")
            i += 1
        else:
            out.append(re.escape(glob[i]))
            i += 1
    out.append("$")
    return re.compile("".join(out))


def rust_files() -> list[str]:
    """Every tracked-looking Rust file, repo-relative, excluding build output."""
    skip = {"target", ".git", ".pixel", "node_modules"}
    found = []
    for path in REPO.rglob("*.rs"):
        rel = path.relative_to(REPO)
        if skip.isdisjoint(rel.parts):
            found.append(rel.as_posix())
    return sorted(found)


class MutantsConfigContract(unittest.TestCase):
    def test_every_excluded_rust_file_is_a_bench_or_a_crate_root_build_script(self):
        patterns = [to_regex(g) for g in exclude_globs()]
        files = rust_files()
        self.assertTrue(files, "found no Rust files to check")
        excluded = [f for f in files if any(p.match(f) for p in patterns)]
        offenders = [
            f for f in excluded if not BENCH.search(f) and not BUILD_SCRIPT.match(f)
        ]
        self.assertEqual(
            offenders,
            [],
            "these files are excluded from the mutation gate but are neither a "
            "bench nor a crate-root build script, so they are modules leaving "
            "the gate unnoticed: " + ", ".join(offenders),
        )

    def test_the_graph_builder_module_is_inside_the_gate(self):
        """The regression this contract was written for.

        `crates/pixel-graph/src/build.rs` is a source module, not a build
        script. A glob that excludes it hides `build_graph`, `tree_delta`,
        `apply_tree_delta` and the freshness signature from the gate.
        """
        module = "crates/pixel-graph/src/build.rs"
        self.assertIn(module, rust_files(), "the module moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        hit = [g for g, p in zip(exclude_globs(), patterns) if p.match(module)]
        self.assertEqual(hit, [], f"{module} is excluded by {hit}")

    def test_the_cli_build_script_stays_excluded(self):
        """The exclusion the config is actually for keeps working."""
        script = "crates/pixel/build.rs"
        self.assertIn(script, rust_files(), "the build script moved; update this test")
        patterns = [to_regex(g) for g in exclude_globs()]
        self.assertTrue(
            any(p.match(script) for p in patterns),
            f"{script} is a cargo build script and should stay excluded",
        )

    def test_the_glob_translation_separates_star_from_double_star(self):
        """`*` must not cross a separator, or the contract above proves nothing."""
        single = to_regex("crates/*/build.rs")
        self.assertTrue(single.match("crates/pixel/build.rs"))
        self.assertFalse(single.match("crates/pixel-graph/src/build.rs"))
        double = to_regex("**/build.rs")
        self.assertTrue(double.match("crates/pixel/build.rs"))
        self.assertTrue(double.match("crates/pixel-graph/src/build.rs"))
        self.assertTrue(double.match("build.rs"))
        self.assertTrue(to_regex("crates/pixel-bench/**").match("crates/pixel-bench/a/b.rs"))
        self.assertFalse(to_regex("crates/pixel-bench/**").match("crates/pixel/a.rs"))


class MutantsGateReport(unittest.TestCase):
    """Contract of scripts/mutants-gate.py.

    The defect it answers: `cargo mutants --in-diff` exits 0 when it produced
    no mutants, so a pull request whose every file is excluded shows a green
    mutation gate over code nothing mutated. #203 did exactly that -- green in
    56 s, `No files were found with the provided path: mutants.out`. The
    report has to separate "0 missed out of N tested" from "0 tested".
    """

    def run_gate(self, diff: str, listing: str, *extra: str):
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-gate-") as tmp:
            root = Path(tmp)
            (root / "pr.diff").write_text(diff)
            (root / "list.txt").write_text(listing)
            return subprocess.run(
                [
                    sys.executable,
                    str(GATE),
                    "--diff",
                    str(root / "pr.diff"),
                    "--list",
                    str(root / "list.txt"),
                    *extra,
                ],
                capture_output=True,
                text=True,
                check=False,
            )

    @staticmethod
    def diff_touching(*paths: str) -> str:
        return "".join(f"--- a/{p}\n+++ b/{p}\n@@ -1 +1 @@\n-a\n+b\n" for p in paths)

    MUTANT_LINE = (
        "crates/pixel-graph/src/build.rs:407:5: replace tree_hashes -> "
        "Vec<(String, u64)> with vec![]\n"
    )

    def test_a_diff_that_produced_mutants_is_reported_as_tested(self):
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"),
            self.MUTANT_LINE * 10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("**mutants tested:** 10", result.stdout)
        self.assertIn("`tested`", result.stdout)
        self.assertNotIn("::warning", result.stdout)

    def test_rust_changed_with_zero_mutants_is_reported_as_vacuous(self):
        """The #203 shape: the gate is green and proves nothing."""
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"), ""
        )
        self.assertIn("**mutants tested:** 0", result.stdout)
        self.assertIn("`vacuous`", result.stdout)
        self.assertIn("proves nothing", result.stdout)
        self.assertIn("crates/pixel-graph/src/build.rs", result.stdout)
        self.assertIn("::warning title=Mutation gate tested nothing::", result.stdout)
        self.assertEqual(result.returncode, 0, "warns by default, never blocks")

    def test_the_vacuous_case_can_be_made_a_hard_failure(self):
        result = self.run_gate(
            self.diff_touching("crates/pixel-graph/src/build.rs"),
            "",
            "--fail-on-vacuous",
        )
        self.assertEqual(result.returncode, 1)

    def test_a_diff_with_no_mutable_rust_owes_no_mutants(self):
        """Docs, benches and crate-root build scripts must not be flagged."""
        for paths in (
            ("README.md", "docs/bench/tree-delta.md"),
            ("crates/pixel-bench/benches/tree_delta.rs",),
            ("crates/pixel/build.rs",),
            ("Cargo.toml", "changelog.d/204-x.fixed.md"),
        ):
            with self.subTest(paths=paths):
                result = self.run_gate(self.diff_touching(*paths), "")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("`not-applicable`", result.stdout)
                self.assertNotIn("::warning", result.stdout)

    def test_a_deleted_file_is_not_counted_as_changed_rust(self):
        deletion = "--- a/crates/pixel-graph/src/gone.rs\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n"
        result = self.run_gate(deletion, "")
        self.assertIn("`not-applicable`", result.stdout)


class ShardedMutantsGate(unittest.TestCase):
    """The two roles scripts/mutants-gate.py plays around the shard matrix.

    Sizing: the plan job turns the listing into `--shard k/n` values. A gap
    in that list is a slice of the diff no job ever mutates, and a value of
    `n/n` makes cargo-mutants refuse to start. Totalling: once the shards
    finish, the gate totals their `outcomes.json`. The failure it exists for
    is a shard that crashed or was cancelled. Its mutants were never judged,
    and without a check the shards that did finish would add up to a pass.
    """

    DIFF = "--- a/crates/pixel/src/main.rs\n+++ b/crates/pixel/src/main.rs\n@@ -1 +1 @@\n-a\n+b\n"

    @staticmethod
    def listing(count: int) -> str:
        return "".join(
            f"crates/pixel/src/main.rs:{line}:5: replace f{line} -> bool with true\n"
            for line in range(1, count + 1)
        )

    @staticmethod
    def write_shard(root: Path, name: str, *summaries: str, logs=None) -> None:
        """One shard's mutants.out, shaped like cargo-mutants 27.1 writes it.

        `logs` maps a mutant's 1-based index to the text of its build log,
        written under `log/` and referenced by `log_path` as cargo-mutants does.
        """
        logs = logs or {}
        (root / name / "log").mkdir(parents=True)
        outcomes = [{"scenario": "Baseline", "summary": "Success"}]
        for i, summary in enumerate(summaries, start=1):
            outcome = {
                "scenario": {"Mutant": {"name": f"crates/pixel/src/main.rs:{i}:5: {name} #{i}"}},
                "summary": summary,
            }
            if i in logs:
                outcome["log_path"] = f"log/mutant_{i}.log"
                (root / name / outcome["log_path"]).write_text(logs[i])
            outcomes.append(outcome)
        (root / name / "outcomes.json").write_text(json.dumps({"outcomes": outcomes}))

    def run_gate(self, listed: int, *extra: str, shards=()):
        """Run the gate on `listed` mutants; `shards` are `(name, summaries)` pairs."""
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-shards-") as tmp:
            root = Path(tmp)
            (root / "pr.diff").write_text(self.DIFF)
            (root / "list.txt").write_text(self.listing(listed))
            for name, summaries, *logs in shards:
                self.write_shard(root / "shards", name, *summaries, logs=logs[0] if logs else None)
            output = root / "github-output"
            result = subprocess.run(
                [
                    sys.executable,
                    str(GATE),
                    "--diff",
                    str(root / "pr.diff"),
                    "--list",
                    str(root / "list.txt"),
                    "--github-output",
                    str(output),
                    *[a.replace("{root}", tmp) for a in extra],
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            outputs = dict(
                line.split("=", 1) for line in output.read_text().splitlines()
            )
            return result, outputs

    def shards_for(self, listed: int) -> list[str]:
        result, outputs = self.run_gate(listed)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(outputs["mutants"], str(listed))
        return json.loads(outputs["shards"])

    def test_a_diff_without_mutants_starts_no_shard(self):
        """Every shard pays a baseline build, so nothing to mutate means no job."""
        self.assertEqual(self.shards_for(0), [])

    def test_shards_cover_every_slice_and_stay_within_cargo_mutants_range(self):
        for listed in (1, 20, 21, 95, 10_000):
            with self.subTest(listed=listed):
                shards = self.shards_for(listed)
                count = len(shards)
                self.assertEqual(shards, [f"{k}/{count}" for k in range(count)])

    def test_a_small_diff_keeps_one_baseline_and_a_large_one_is_split(self):
        self.assertEqual(len(self.shards_for(20)), 1)
        self.assertGreater(len(self.shards_for(95)), 1)

    def test_a_huge_diff_does_not_exceed_the_concurrent_job_budget(self):
        """A free account runs 20 jobs at once; the CI workflow needs some of them."""
        self.assertLessEqual(len(self.shards_for(10_000)), 20)

    def test_every_listed_mutant_caught_or_unviable_passes(self):
        result, _ = self.run_gate(
            4,
            "--outcomes-root",
            "{root}/shards",
            shards=[
                ("mutants-out-0", ("CaughtMutant", "Unviable")),
                ("mutants-out-1", ("CaughtMutant", "CaughtMutant")),
            ],
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("4 judged: 3 caught, 0 missed, 0 timeout, 1 unviable", result.stdout)
        self.assertIn("**gate:** passed", result.stdout)

    def test_a_single_shard_extracted_at_the_root_is_totalled(self):
        """`download-artifact` puts a lone matching artifact straight into its path.

        With one shard there is no `shards/mutants-out-0/` directory: the
        artifact's files land in `shards/` itself. Reading only
        `shards/*/outcomes.json` then totals nothing, and every PR of 20
        mutants or fewer failed with "0 reached a verdict" (#225).
        """
        with tempfile.TemporaryDirectory(prefix="pixel-mutants-shards-") as tmp:
            self.write_shard(Path(tmp), "shards", "CaughtMutant", "Unviable")
            result, _ = self.run_gate(2, "--outcomes-root", f"{tmp}/shards")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("2 judged: 1 caught, 0 missed, 0 timeout, 1 unviable", result.stdout)

    def test_a_survivor_in_any_shard_fails_the_gate_and_is_named(self):
        for summary, label in (("MissedMutant", "MISSED"), ("Timeout", "TIMEOUT")):
            with self.subTest(summary=summary):
                result, _ = self.run_gate(
                    3,
                    "--outcomes-root",
                    "{root}/shards",
                    shards=[
                        ("mutants-out-0", ("CaughtMutant",)),
                        ("mutants-out-1", ("CaughtMutant", summary)),
                    ],
                )
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"{label} crates/pixel/src/main.rs:2:5: mutants-out-1 #2", result.stdout)
                self.assertIn("::error title=Mutation gate failed::", result.stdout)

    def test_an_outcome_the_gate_does_not_know_fails_closed(self):
        """A summary a later cargo-mutants adds must not count as caught."""
        result, _ = self.run_gate(
            2,
            "--outcomes-root",
            "{root}/shards",
            shards=[("mutants-out-0", ("CaughtMutant", "Failure"))],
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn("1 mutant(s) ended with an outcome the gate does not know (Failure)", result.stdout)
        self.assertIn("FAILURE crates/pixel/src/main.rs:2:5: mutants-out-0 #2", result.stdout)
        self.assertIn("**gate:** failed", result.stdout)

    def test_a_shard_that_left_no_outcomes_fails_the_gate(self):
        """Two shards caught everything they ran; the third never reported."""
        result, _ = self.run_gate(
            6,
            "--outcomes-root",
            "{root}/shards",
            shards=[
                ("mutants-out-0", ("CaughtMutant", "CaughtMutant")),
                ("mutants-out-1", ("CaughtMutant", "CaughtMutant")),
            ],
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("6 mutant(s) listed, 4 reached a verdict", result.stdout)

    def test_a_shard_whose_baseline_failed_counts_as_unjudged(self):
        result, _ = self.run_gate(
            2, "--outcomes-root", "{root}/shards", shards=[("mutants-out-0", ())]
        )
        self.assertEqual(result.returncode, 1)
        self.assertIn("2 mutant(s) listed, 0 reached a verdict", result.stdout)

    def test_the_baseline_is_not_counted_as_a_mutant(self):
        """Counting it would turn every complete run into a mismatch."""
        result, _ = self.run_gate(
            1,
            "--outcomes-root",
            "{root}/shards",
            shards=[("mutants-out-0", ("CaughtMutant",))],
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 judged", result.stdout)

    def test_an_unviable_mutant_whose_build_ran_out_of_disk_was_never_judged(self):
        """PR #222's run filled the runner's disk at its 101st mutant.

        Every later build failed at the link step, cargo-mutants reported
        each one `unviable`, and a gate that holds `unviable` called a run
        whose last 546 mutants never compiled `tested`. A genuine unviable
        (a rustc type error) in the same shard must still pass.
        """
        result, _ = self.run_gate(
            3,
            "--outcomes-root",
            "{root}/shards",
            shards=[(
                "mutants-out-0",
                ("CaughtMutant", "Unviable", "Unviable"),
                {
                    2: "error[E0277]: the trait bound `Response: Default` is not satisfied\n",
                    3: "cc: error: Cannot create temporary file in /tmp/: No space left on device\n",
                },
            )],
        )
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn(
            "3 judged: 1 caught, 0 missed, 0 timeout, 1 unviable, 1 disk-full", result.stdout
        )
        self.assertIn("1 mutant(s) reported unviable because the runner's disk filled up", result.stdout)
        self.assertIn("DISK-FULL crates/pixel/src/main.rs:3:5: mutants-out-0 #3", result.stdout)
        self.assertNotIn("#2", result.stdout)

    def test_an_unviable_mutant_without_a_log_stays_unviable(self):
        """No log is no evidence of a full disk: the outcome is taken as reported."""
        result, _ = self.run_gate(
            1, "--outcomes-root", "{root}/shards", shards=[("mutants-out-0", ("Unviable",))]
        )
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("1 judged: 0 caught, 0 missed, 0 timeout, 1 unviable, 0 disk-full", result.stdout)

    @unittest.skipUnless(
        os.environ.get("MUTANTS_REPLAY_PR222"),
        "set MUTANTS_REPLAY_PR222 to `gh run download 35852843025 -n mutants-out -D <dir>`",
    )
    def test_the_pr222_run_replays_as_546_mutants_lost_to_a_full_disk(self):
        """The run that motivated the check, replayed from its own artifact."""
        spec = importlib.util.spec_from_file_location("mutants_gate", GATE)
        gate = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(gate)
        counts, _ = gate.tally(Path(os.environ["MUTANTS_REPLAY_PR222"]))
        self.assertEqual(counts[gate.DISK_FULL], 546)
        self.assertEqual(counts["unviable"], 20)
        self.assertEqual((counts["caught"], counts["missed"], counts["timeout"]), (67, 24, 1))
        self.assertIn("disk filled up", gate.outcome_failure(658, counts))

    def test_no_mutants_and_no_shards_is_not_a_failure(self):
        result, _ = self.run_gate(0, "--outcomes-root", "{root}/shards")
        self.assertEqual(result.returncode, 0, result.stdout)
        self.assertIn("**gate:** passed", result.stdout)


if __name__ == "__main__":
    unittest.main(verbosity=2)
