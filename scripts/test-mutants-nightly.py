#!/usr/bin/env python3
"""Contract of scripts/mutants-nightly.py: the rotation and the tracking issue.

What must hold: the seven nights together cover every shard of the
whole-tree list exactly once, the count a night expects is the count its
shards receive (otherwise a crashed shard passes as a complete slice), a
survivor or an unjudged mutant makes the night red, and rewriting one
night's section of the issue leaves the other nights' sections intact.
"""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).with_name("mutants-nightly.py")
_spec = importlib.util.spec_from_file_location("mutants_nightly", SCRIPT)
nightly = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(nightly)


def outcomes(root: Path, shard: str, summaries: list[tuple[str, str]]) -> None:
    """One shard's mutants.out, shaped like cargo-mutants 27.1 writes it."""
    out = root / shard
    out.mkdir(parents=True)
    scenarios = [{"scenario": "Baseline", "summary": "Success"}]
    scenarios += [{"scenario": {"Mutant": {"name": name}}, "summary": summary}
                  for name, summary in summaries]
    (out / "outcomes.json").write_text(json.dumps({"outcomes": scenarios}))


class Rotation(unittest.TestCase):
    def test_the_week_covers_every_shard_exactly_once(self):
        shards = [s for d in range(nightly.SLICES) for s in nightly.slice_shards(d)]
        self.assertEqual(len(shards), len(set(shards)))
        self.assertEqual(sorted(int(s.split("/")[0]) for s in shards), list(range(nightly.TOTAL_SHARDS)))
        self.assertTrue(all(s.endswith(f"/{nightly.TOTAL_SHARDS}") for s in shards))

    def test_the_week_expects_every_mutant_exactly_once(self):
        for total in (0, 1, 69, 70, 71, 13_624):
            with self.subTest(total=total):
                self.assertEqual(sum(nightly.expected_mutants(total, d) for d in range(nightly.SLICES)), total)

    def test_a_night_expects_what_round_robin_gives_its_shards(self):
        # 13 624 = 194 * 70 + 44: shards 0..43 get 195 mutants, 44..69 get 194.
        self.assertEqual(nightly.expected_mutants(13_624, 0), 1950)
        self.assertEqual(nightly.expected_mutants(13_624, 4), 4 * 195 + 6 * 194)
        self.assertEqual(nightly.expected_mutants(13_624, 6), 1940)

    def test_a_slice_outside_the_week_is_refused(self):
        with self.assertRaises(ValueError):
            nightly.slice_shards(nightly.SLICES)

    def test_plan_maps_the_weekday_and_writes_the_matrix(self):
        with tempfile.TemporaryDirectory() as tmp:
            listing = Path(tmp, "list.txt")
            listing.write_text("".join(f"crates/a/src/lib.rs:{i}:1: replace f -> u8 with 0\n" for i in range(140)))
            out = Path(tmp, "out")
            self.assertEqual(nightly.main(["plan", "--weekday", "2", "--list", str(listing), "--github-output", str(out)]), 0)
            written = dict(line.split("=", 1) for line in out.read_text().splitlines())
        self.assertEqual(written["slice"], "2")
        self.assertEqual(json.loads(written["shards"]), [f"{k}/70" for k in range(20, 30)])
        self.assertEqual(written["expected"], "20")
        self.assertEqual(written["total"], "140")


class Report(unittest.TestCase):
    def run_report(self, root: Path, expected: int, body: str = "") -> tuple[int, str]:
        issue = root.parent / "issue.md"
        issue.write_text(body)
        out = root.parent / "new.md"
        import contextlib, io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = nightly.main(["report", "--slice", "3", "--expected", str(expected),
                                 "--outcomes-root", str(root), "--run-url", "https://example/run/1",
                                 "--sha", "0123456789abcdef", "--issue-body", str(issue)])
        out.write_text(buf.getvalue())
        return code, buf.getvalue()

    def test_a_held_slice_is_green_and_says_so(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant"), ("g", "Unviable")])
            code, body = self.run_report(root, 2)
        self.assertEqual(code, 0)
        self.assertIn("### Slice 3 (held)", body)
        self.assertIn("2 mutant(s) assigned, 2 judged (1 caught, 1 unviable)", body)

    def test_a_survivor_makes_the_night_red_and_is_named(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            outcomes(root, "b", [("crates/x.rs:3:1: replace g -> bool with true", "MissedMutant")])
            code, body = self.run_report(root, 2)
        self.assertEqual(code, 1)
        self.assertIn("MISSED crates/x.rs:3:1: replace g -> bool with true", body)
        self.assertIn("(needs work)", body)

    def test_a_shard_that_never_reported_leaves_the_night_red(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            code, body = self.run_report(root, 195)
        self.assertEqual(code, 1)
        self.assertIn("195 mutant(s) listed, 1 reached a verdict", body)

    def test_no_artifact_at_all_is_a_red_night_not_a_crash(self):
        with tempfile.TemporaryDirectory() as tmp:
            code, body = self.run_report(Path(tmp, "missing"), 10)
        self.assertEqual(code, 1)
        self.assertIn("0 judged (no outcome)", body)

    def test_the_named_survivors_are_capped_and_the_rest_counted(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            n = nightly.MAX_NAMED + 7
            outcomes(root, "a", [(f"m{i}", "MissedMutant") for i in range(n)])
            _, body = self.run_report(root, n)
        self.assertEqual(body.count("MISSED m"), nightly.MAX_NAMED)
        self.assertIn("7 more in the run's", body)

    def test_rewriting_one_night_keeps_the_others(self):
        other = nightly.section(1, 5, nightly.Counter(caught=5), [], None, "https://example/run/0", "f" * 40)
        stale = nightly.section(3, 9, nightly.Counter(missed=9), ["MISSED old"], "stale", "https://example/run/-1", "e" * 40)
        body = nightly.update_body(nightly.update_body("", 1, other), 3, stale)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp, "shards")
            outcomes(root, "a", [("f", "CaughtMutant")])
            _, new = self.run_report(root, 1, body)
        self.assertIn(other, new)
        self.assertNotIn("MISSED old", new)
        self.assertEqual(new.count("<!-- mutants-nightly slice 3 -->"), 1)
        self.assertLess(new.index("slice 1 -->"), new.index("slice 3 -->"))
        self.assertEqual(new.count(nightly.HEADER), 1)

    def test_a_new_night_is_inserted_in_slice_order(self):
        body = ""
        for d in (5, 0, 3):
            body = nightly.update_body(body, d, nightly.section(d, 0, nightly.Counter(), [], None, "u", "s" * 12))
        order = [int(x) for x in __import__("re").findall(r"<!-- mutants-nightly slice (\d+) -->", body)]
        self.assertEqual(order, [0, 3, 5])


if __name__ == "__main__":
    unittest.main(verbosity=2)
