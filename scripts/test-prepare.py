#!/usr/bin/env python3
"""Contract of .agents/skills/release/prepare.sh: the pull requests it lists.

Runs the real script inside a disposable workspace repository with stub
`cargo` and `gh` on PATH. The stub `gh` answers `pr list` with the pull
requests of the fixture when they are asked for on `main`, whatever the date
search says (GitHub's own search returned the previous prepare PR), and applies the script's `--jq` expression
with the real `jq`, so the expression is exercised too.
"""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

PREPARE = Path(__file__).resolve().parent.parent / ".agents/skills/release/prepare.sh"
MANIFESTS = [
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
    ".devin-plugin/plugin.json",
    ".qoder-plugin/plugin.json",
    "gemini-extension.json",
    "package.json",
]

GH = """#!/usr/bin/env python3
import json, os, subprocess, sys
args = sys.argv[1:]
if args[:2] == ["pr", "list"]:
    # Pull requests merge into main: a listing on any other base (the retired
    # develop) finds none, so a script still asking for it lists nothing.
    base = args[args.index("--base") + 1] if "--base" in args else None
    prs = json.loads(open(os.environ["FIXTURE_PRS"]).read()) if base == "main" else []
    expr = args[args.index("--jq") + 1]
    out = subprocess.run(["jq", "-r", expr], input=json.dumps(prs),
                         capture_output=True, text=True, check=True)
    sys.stdout.write(out.stdout)
elif args[:2] == ["repo", "view"]:
    print("example/fixture")
elif args[:1] == ["api"]:
    print("1")
else:
    sys.exit(1)
"""


class PrepareContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-prepare-contract-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        fake = self.root / "fake"
        fake.mkdir()
        (fake / "gh").write_text(GH)
        (fake / "cargo").write_text("#!/bin/sh\nexit 0\n")
        for stub in fake.iterdir():
            stub.chmod(0o755)
        self.prs = self.root / "prs.json"
        self.env = {
            **os.environ,
            "PATH": str(fake) + os.pathsep + os.environ["PATH"],
            "FIXTURE_PRS": str(self.prs),
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_AUTHOR_NAME": "t",
            "GIT_AUTHOR_EMAIL": "t@example.com",
            "GIT_COMMITTER_NAME": "t",
            "GIT_COMMITTER_EMAIL": "t@example.com",
        }
        self.write("Cargo.toml", '[workspace]\nmembers = ["crates/a"]\n')
        self.write("crates/a/Cargo.toml", '[package]\nname = "a"\nversion = "0.1.0"\n')
        for manifest in MANIFESTS:
            self.write(manifest, '{\n  "name": "a",\n  "version": "0.1.0"\n}\n')
        self.write("plugin.yaml", "name: a\nversion: 0.1.0\n")
        self.write("scripts/gen-plugin-assets.sh", "#!/bin/sh\n")
        self.write("CHANGELOG.md", "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n")
        self.write("changelog.d/.gitkeep", "")
        self.git("init", "-q", "-b", "main")
        self.git("add", ".")
        self.git("commit", "-qm", "base")

    def write(self, rel, text):
        path = self.repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def git(self, *args):
        return subprocess.run(
            ["git", "-C", str(self.repo), *args],
            env=self.env,
            check=True, capture_output=True, text=True,
        ).stdout.strip()

    def merge_pr(self, branch, rel):
        """A pull request merged into main with a merge commit."""
        self.git("switch", "-q", "-c", branch)
        self.write(rel, branch + "\n")
        self.git("add", ".")
        self.git("commit", "-qm", branch)
        self.git("switch", "-q", "main")
        self.git("merge", "-q", "--no-ff", "-m", "Merge " + branch, branch)
        return self.git("rev-parse", "HEAD")

    def prepare(self):
        shutil.copy(PREPARE, self.repo / "prepare.sh")
        self.git("add", "prepare.sh")
        self.git("commit", "-qm", "script")
        return subprocess.run(
            ["sh", "prepare.sh", "0.2.0", "--date", "2026-02-01"],
            cwd=self.repo, env=self.env, capture_output=True, text=True, timeout=30,
        )

    def changelog(self):
        return (self.repo / "CHANGELOG.md").read_text()

    def fragments(self):
        return sorted(p.name for p in (self.repo / "changelog.d").glob("*.md"))

    def listed(self, stdout):
        head = "pull requests merged into main since v0.1.0"
        lines = stdout.splitlines()
        start = next(i for i, line in enumerate(lines) if line.startswith(head)) + 1
        block = []
        for line in lines[start:]:
            if not line.startswith("  "):
                break
            block.append(line.strip())
        return block

    def test_the_pull_request_the_tag_merged_is_not_listed_as_unreleased(self):
        released = self.merge_pr("release-0.1.0", "released.txt")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0", released)
        fixed = self.merge_pr("fix-thing", "fixed.txt")
        self.write("changelog.d/12-fix-thing.fixed.md", "thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")
        self.prs.write_text(json.dumps([
            {"number": 12, "title": "fix: thing", "mergeCommit": {"oid": fixed}},
            {"number": 11, "title": "release: prepare 0.1.0", "mergeCommit": {"oid": released}},
            # Merged on GitHub, merge commit not fetched into this clone.
            {"number": 13, "title": "fix: elsewhere", "mergeCommit": {"oid": "1" * 40}},
        ]))

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["#12 fix: thing", "#13 fix: elsewhere"])

    def test_nothing_unreleased_says_none(self):
        released = self.merge_pr("release-0.1.0", "released.txt")
        self.git("tag", "-a", "v0.1.0", "-m", "v0.1.0", released)
        self.write("changelog.d/11-thing.fixed.md", "thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")
        self.prs.write_text(json.dumps([
            {"number": 11, "title": "release: prepare 0.1.0", "mergeCommit": {"oid": released}},
        ]))

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["(none)"])

    def test_the_fragments_become_the_release_section_by_section(self):
        """One fragment per entry, filed under the heading its name names."""
        self.write("changelog.d/12-add-a-flag.added.md", "`pixel thing --flag` is new.\n")
        self.write("changelog.d/13-fix-a-thing.fixed.md", "`pixel thing` no longer breaks.\n")
        self.write("changelog.d/14-change-a-thing.changed.md", "`pixel thing` says less.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragments")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        changelog = self.changelog()
        # A kept, empty Unreleased, then the release heading, then the
        # sections grouped, in the order the file has always used.
        self.assertTrue(changelog.startswith(
            "# Changelog\n\n## [Unreleased]\n\n## [0.2.0] - 2026-02-01\n"), changelog)
        self.assertEqual(
            [line for line in changelog.splitlines() if line.startswith("### ")],
            ["### Added", "### Changed", "### Fixed"],
        )
        for entry in ["- `pixel thing --flag` is new.",
                      "- `pixel thing` says less.",
                      "- `pixel thing` no longer breaks."]:
            self.assertIn(entry + "\n", changelog)
        # The released entries leave the fragments behind them.
        self.assertEqual(self.fragments(), [])
        # The cut inserts a section, it does not swallow the ones after it:
        # Unreleased stays on top, the new section under it, the history last.
        self.assertLess(changelog.index("## [Unreleased]"), changelog.index("## [0.2.0]"))
        self.assertLess(
            changelog.index("## [0.2.0]"), changelog.index("## [0.1.0] - 2026-01-01")
        )

    def test_a_long_entry_keeps_every_line(self):
        """A wrapped entry keeps its continuation lines, indented under the bullet."""
        self.write("changelog.d/12-long.fixed.md", "first line\ncontinued here\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("\n- first line\n  continued here\n", self.changelog())

    def test_a_fragment_without_a_known_section_is_refused(self):
        self.write("changelog.d/12-thing.fized.md", "thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)
        self.assertIn("fixed", result.stderr)

    def test_a_nameless_fragment_is_refused(self):
        self.write("changelog.d/12-thing.md", "thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)

    def test_an_empty_fragment_is_refused(self):
        self.write("changelog.d/12-thing.fixed.md", "")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is empty", result.stderr)

    def test_a_whitespace_only_fragment_is_refused(self):
        """`-s` is not enough: a file of newlines would file a bare `- `."""
        for name, body in (("12-newline.fixed.md", "\n"),
                           ("13-spaces.fixed.md", "   \n"),
                           ("14-blanks.fixed.md", "\n\n\n")):
            with self.subTest(name=name):
                self.write("changelog.d/" + name, body)
                self.git("add", ".")
                self.git("commit", "-qm", "fragment")

                result = self.prepare()

                self.assertNotEqual(result.returncode, 0)
                self.assertIn("is empty", result.stderr)
                self.git("reset", "-q", "--hard", "HEAD~1")

    def test_a_fragment_whose_first_line_is_blank_is_refused(self):
        """The first line is the bullet; a blank one files `- ` and an indent."""
        self.write("changelog.d/12-thing.fixed.md", "\nthe entry on the second line\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("starts with a blank line", result.stderr)

    def test_an_entry_left_under_unreleased_is_refused(self):
        self.write("changelog.d/12-thing.fixed.md", "thing\n")
        self.write("CHANGELOG.md",
                   "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- written straight into the file\n\n## [0.1.0] - 2026-01-01\n")
        self.git("add", ".")
        self.git("commit", "-qm", "stray")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bullet(s) still under ## [Unreleased]", result.stderr)
        self.assertIn("entries live in changelog.d/", result.stderr)
        # Refused before any write: the stray line is still there, and so is
        # the fragment nobody cut.
        self.assertIn("- written straight into the file", self.changelog())
        self.assertEqual(self.fragments(), ["12-thing.fixed.md"])

    def test_no_fragment_means_nothing_to_release(self):
        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_a_version_check_still_runs_before_the_fragments_are_touched(self):
        self.write("changelog.d/12-thing.fixed.md", "thing\n")
        self.write("CHANGELOG.md",
                   "# Changelog\n\n## [Unreleased]\n\n## [0.2.0] - 2026-01-02\n\n## [0.1.0] - 2026-01-01\n")
        self.git("add", ".")
        self.git("commit", "-qm", "heading")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already has a ## [0.2.0] heading", result.stderr)
        self.assertEqual(self.fragments(), ["12-thing.fixed.md"])

    def test_a_fragment_written_before_the_cut_waits_for_the_next_release(self):
        """The point of the directory, and what CHANGELOG.md could not do.

        A branch cut before the release carries an entry that does not belong
        to it. Written straight into CHANGELOG.md, a three-way merge puts that
        entry under the heading of the release that has just shipped -- no
        conflict, and a rebase does the same -- so the entry is published in a
        release it was never part of. A fragment is simply not in the cut.
        """
        self.write("changelog.d/12-shipped.fixed.md", "shipped before the cut\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment to release")
        self.git("switch", "-q", "-c", "in-flight")
        self.write("changelog.d/13-later.fixed.md", "for the release after this one\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment for the next release")

        self.git("switch", "-q", "main")
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # The script never commits: the maintainer does, reviewing the diff.
        self.git("add", "-A")
        self.git("commit", "-qm", "release: prepare 0.2.0")
        self.git("merge", "-q", "--no-ff", "-m", "Merge in-flight", "in-flight")

        changelog = self.changelog()
        released = changelog.split("## [0.2.0]")[1].split("## [0.1.0]")[0]
        self.assertIn("shipped before the cut", released)
        self.assertNotIn("for the release after this one", released)
        # Still a fragment, waiting for the release that will fold it in.
        self.assertEqual(self.fragments(), ["13-later.fixed.md"])
        self.assertIn("## [Unreleased]", changelog)


class FragmentContract(unittest.TestCase):
    """`prepare.sh --check` accepts what this repository actually ships.

    It is what turns a mistyped section in a pull request into a red check on
    that pull request, instead of a release that refuses to tag.
    """

    ROOT = PREPARE.parent.parent.parent.parent

    def run_check(self, root):
        return subprocess.run(
            ["sh", str(PREPARE), "--check"],
            cwd=root, capture_output=True, text=True, timeout=30,
        )

    CHANGELOG = "# Changelog\n\n## [Unreleased]\n\n## [0.1.0] - 2026-01-01\n"

    def make_repo(self, fragments, changelog=None):
        """A bare repository holding only what --check reads."""
        tmp = tempfile.TemporaryDirectory(prefix="pixel-prepare-check-")
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        (root / "changelog.d").mkdir()
        for name, text in fragments.items():
            (root / "changelog.d" / name).write_text(text)
        (root / "CHANGELOG.md").write_text(self.CHANGELOG if changelog is None else changelog)
        subprocess.run(["git", "init", "-q", "-b", "main"], cwd=root, check=True)
        return root

    def test_the_repository_fragments_are_well_formed(self):
        result = self.run_check(self.ROOT)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("well formed", result.stdout)
        self.assertIn("## [Unreleased] empty", result.stdout)

    def test_check_accepts_the_empty_directory_a_release_leaves_behind(self):
        """A release pull request is the one that empties changelog.d/.

        The cut deletes every fragment, so the commit `--check` runs on has an
        empty directory. While the no-fragment refusal sat above the `--check`
        return it failed that pull request -- the release of 0.4.0 went red on
        its own preparation -- and the only way to green it was to stop cutting
        the release or to write a fragment nobody had an entry for.
        """
        root = self.make_repo({})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("well formed", result.stdout)
        self.assertIn("## [Unreleased] empty", result.stdout)

    def test_the_cut_still_refuses_the_empty_directory_check_accepts(self):
        """`--check` accepting it must not make the cut accept it too.

        Tagging a version whose changelog section would be empty is the thing
        the refusal exists for; only the validator had to stop sharing it.
        """
        root = self.make_repo({})
        result = subprocess.run(
            ["sh", str(PREPARE), "9.9.9"],
            cwd=root, capture_output=True, text=True, timeout=30,
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_check_writes_nothing(self):
        before = {p: p.read_bytes() for p in sorted(self.ROOT.glob("changelog.d/*.md"))}
        self.run_check(self.ROOT)
        self.assertEqual(
            before,
            {p: p.read_bytes() for p in sorted(self.ROOT.glob("changelog.d/*.md"))},
        )

    def test_check_refuses_a_repository_whose_fragments_are_misnamed(self):
        root = self.make_repo({"thing.fized.md": "thing\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)

    def test_check_refuses_a_whitespace_only_fragment(self):
        root = self.make_repo({"12-thing.fixed.md": "\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is empty", result.stderr)

    def test_check_does_not_report_unreleased_empty_without_looking(self):
        """The success message claims something; it has to have checked it.

        `--check` used to return before the stray-bullet test, so a tree that
        release preparation refuses was reported as well formed.
        """
        root = self.make_repo(
            {"12-thing.fixed.md": "thing\n"},
            changelog="# Changelog\n\n## [Unreleased]\n\n### Fixed\n- stray\n\n## [0.1.0] - 2026-01-01\n",
        )
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bullet(s) still under ## [Unreleased]", result.stderr)
        self.assertNotIn("all well formed", result.stdout)


if __name__ == "__main__":
    unittest.main()
