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
        self.write("changelog.d/12-fix-thing.fixed.md", "**thing:** thing\n")
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
        self.write("changelog.d/11-thing.fixed.md", "**thing:** thing\n")
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
        self.write("changelog.d/12-add-a-flag.added.md", "**thing:** `pixel thing --flag` is new.\n")
        self.write("changelog.d/13-fix-a-thing.fixed.md", "**thing:** `pixel thing` no longer breaks.\n")
        self.write("changelog.d/14-change-a-thing.changed.md", "**thing:** `pixel thing` says less.\n")
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
        for entry in ["- **thing:** `pixel thing --flag` is new.",
                      "- **thing:** `pixel thing` says less.",
                      "- **thing:** `pixel thing` no longer breaks."]:
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
        self.write("changelog.d/12-long.fixed.md", "**thing:** first line\ncontinued here\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("\n- **thing:** first line\n  continued here\n", self.changelog())

    def test_the_highlights_lead_the_released_section(self):
        """The release narrative, once per release instead of once per entry.

        With nowhere to put "what this release is about", every entry carries
        a sentence of it: that is how 0.4.0 reached a median bullet of 715
        bytes. It goes above the sections because release.yml cuts the GitHub
        release body from the version heading to the next `## `, so the body
        opens on it.
        """
        self.write("changelog.d/_highlights.md",
                   "This release is about the changelog.\n\n### Highlights\n- one thing.\n")
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragments")

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(
            "## [0.2.0] - 2026-02-01\n\nThis release is about the changelog.\n"
            "\n### Highlights\n- one thing.\n\n### Fixed\n- **thing:** thing\n",
            self.changelog(),
        )
        # It is consumed by the cut like any fragment: left behind, it would
        # lead the next release with the previous release's narrative.
        self.assertEqual(self.fragments(), [])
        self.assertIn("led by changelog.d/_highlights.md", result.stdout)

    def test_the_highlights_are_not_counted_as_an_entry(self):
        """A chapeau is not a changelog entry: alone, there is nothing to release.

        Counted as one, it would let a release be cut whose section holds a
        narrative and no bullet -- and be filed under a section its name does
        not have.
        """
        self.write("changelog.d/_highlights.md", "Only a narrative.\n")
        self.git("add", ".")
        self.git("commit", "-qm", "highlights only")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("changelog.d/ holds no fragment", result.stderr)

    def test_a_fragment_without_a_known_section_is_refused(self):
        self.write("changelog.d/12-thing.fized.md", "**thing:** thing\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment")

        result = self.prepare()

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)
        self.assertIn("fixed", result.stderr)

    def test_a_nameless_fragment_is_refused(self):
        self.write("changelog.d/12-thing.md", "**thing:** thing\n")
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
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
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
        self.write("changelog.d/12-thing.fixed.md", "**thing:** thing\n")
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
        self.write("changelog.d/12-shipped.fixed.md", "**thing:** shipped before the cut\n")
        self.git("add", ".")
        self.git("commit", "-qm", "fragment to release")
        self.git("switch", "-q", "-c", "in-flight")
        self.write("changelog.d/13-later.fixed.md", "**thing:** for the release after this one\n")
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
        root = self.make_repo({"thing.fized.md": "**thing:** thing\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("name it <slug>.<section>.md", result.stderr)

    def test_check_refuses_a_whitespace_only_fragment(self):
        root = self.make_repo({"12-thing.fixed.md": "\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("is empty", result.stderr)

    def entry(self, length, prefix="**thing:** ", suffix=" (#12)"):
        """An entry of exactly `length` bytes, scope and link included."""
        body = "x" * (length - len(prefix) - len(suffix))
        return prefix + body + suffix + "\n"

    def test_check_refuses_an_entry_that_does_not_open_on_its_scope(self):
        """The scope is what makes a released section scannable.

        0.4.0 shipped twelve bullets with none, so finding the entry about a
        given command means reading every one of them to the first backtick.
        """
        root = self.make_repo({"12-thing.fixed.md": "`pixel thing` no longer breaks.\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("open the entry with the scope it changes", result.stderr)
        self.assertIn("**graph:**", result.stderr)

    def test_check_accepts_a_scope_naming_more_than_one_area(self):
        """A change landing in two places still has one scope line."""
        root = self.make_repo({"12-thing.fixed.md": "**graph, daemon:** it no longer breaks. (#12)\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_check_refuses_an_entry_over_the_cap(self):
        """The cap is the whole point: the reasoning belongs to the pull request.

        The fragment this gate was written for ran 1428 bytes in one paragraph,
        most of it arguing for the threshold it picked -- an argument the pull
        request already carried, and that a reader of the changelog is not
        looking for.
        """
        root = self.make_repo({"12-thing.fixed.md": self.entry(901)})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("901 bytes, over the 900 cap", result.stderr)
        self.assertIn("leave the reasoning to the pull request", result.stderr)

    def test_the_cap_measures_the_entry_and_not_its_first_line(self):
        """A wrapped fragment is one entry; the cut reflows it under one bullet.

        Measuring the first line alone would let the same prose through by
        pressing the return key.
        """
        wrapped = self.entry(901).replace("xxxxxxxxxx", "xxxxx\nxxxxx", 1)
        self.assertIn("\n", wrapped.strip())
        root = self.make_repo({"12-thing.fixed.md": wrapped})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("over the 900 cap", result.stderr)

    def test_an_entry_over_the_style_length_warns_without_refusing(self):
        """Between the two limits the entry ships, and the author is told.

        A hard cap alone would make 900 bytes the target; the warning is what
        keeps 500 the one, without refusing the entry that genuinely carries a
        before/after measurement.
        """
        root = self.make_repo({"12-thing.fixed.md": self.entry(501)})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("501 bytes, over the 500", result.stderr)
        self.assertIn("not a refusal", result.stderr)
        self.assertIn("well formed", result.stdout)

    def test_an_entry_referencing_no_pull_request_is_refused(self):
        """A short entry needs somewhere to send the reader for the rest.

        Cutting the reasoning out of the entry is only an improvement while the
        reasoning is still reachable. It used to be a warning, and three
        entries in a row (#253-#255) merged without their link because nothing
        read it before the release; this refusal is what turns the
        pull request's own run red while its number is known, and the message
        is the fix.
        """
        root = self.make_repo({"thing.fixed.md": "**thing:** it no longer breaks.\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("thing.fixed.md: no pull request referenced", result.stderr)
        self.assertIn("git mv changelog.d/thing.fixed.md changelog.d/<number>-thing.fixed.md", result.stderr)
        self.assertIn("https://github.com/LivioGama/pixel/pull/<number>", result.stderr)

    def test_an_issue_number_is_not_a_pull_request_reference(self):
        """`#42` alone may be an issue: the text counts only with the URL."""
        root = self.make_repo({"thing.fixed.md": "**thing:** it no longer breaks (fixes issue #42).\n"})
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("no pull request referenced", result.stderr)

    def test_a_link_in_the_entry_references_the_pull_request(self):
        """The link alone is enough, whatever the slug."""
        root = self.make_repo({
            "thing.fixed.md": "**thing:** it no longer breaks. ([#12](https://github.com/LivioGama/pixel/pull/12))\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("no pull request referenced", result.stderr)

    def test_a_number_first_in_the_slug_references_the_pull_request(self):
        """The convention the directory already had counts as the reference."""
        root = self.make_repo({"12-thing.fixed.md": "**thing:** it no longer breaks.\n"})
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertNotIn("no pull request referenced", result.stderr)

    def test_check_reports_the_highlights_next_to_the_entries(self):
        root = self.make_repo({
            "_highlights.md": "A narrative.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("1 fragment(s) under changelog.d/ plus _highlights.md", result.stdout)

    def test_check_refuses_a_misspelt_highlights_file(self):
        """Skipping it silently would drop the chapeau from its own release.

        `_highlights.md` is the one underscore name the directory takes, so
        anything else with that prefix is a typo of it, not a new convention.
        """
        root = self.make_repo({
            "_highlight.md": "A narrative.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("the only underscore-named file changelog.d/ takes is _highlights.md",
                      result.stderr)

    def test_check_refuses_a_heading_that_would_end_the_release_body(self):
        """release.yml cuts the body from `## [x.y.z]` to the next `## `.

        A `##` heading inside the chapeau truncates the release notes there,
        dropping every section under it -- the entries included.
        """
        for heading in ("## Highlights", "# Highlights"):
            with self.subTest(heading=heading):
                root = self.make_repo({
                    "_highlights.md": "A narrative.\n\n" + heading + "\n- one thing.\n",
                    "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
                })
                result = self.run_check(root)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("use `###` and below", result.stderr)

    def test_check_accepts_a_third_level_heading_in_the_highlights(self):
        root = self.make_repo({
            "_highlights.md": "A narrative.\n\n### Highlights\n- one thing.\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_check_refuses_highlights_over_the_cap(self):
        """The cap is what stops the prose moving from the entries into here."""
        root = self.make_repo({
            "_highlights.md": "x" * 2001 + "\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("2001 bytes, over the 2000 cap", result.stderr)

    def test_check_refuses_an_empty_highlights_file(self):
        """Absent is a valid release; present and empty is a forgotten one."""
        root = self.make_repo({
            "_highlights.md": "\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("_highlights.md is empty", result.stderr)

    def test_the_highlights_are_not_held_to_the_entry_style(self):
        """It is a paragraph, not a bullet: no scope prefix, no 500-byte aim."""
        root = self.make_repo({
            "_highlights.md": "A narrative of " + "x" * 600 + ".\n",
            "12-thing.fixed.md": "**thing:** it no longer breaks.\n",
        })
        result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        # Seen, and held to its own limits rather than the entry's.
        self.assertIn("plus _highlights.md", result.stdout)
        self.assertNotIn("open the entry with the scope", result.stderr)
        self.assertNotIn("the style aims at", result.stderr)

    def test_check_does_not_report_unreleased_empty_without_looking(self):
        """The success message claims something; it has to have checked it.

        `--check` used to return before the stray-bullet test, so a tree that
        release preparation refuses was reported as well formed.
        """
        root = self.make_repo(
            {"12-thing.fixed.md": "**thing:** thing\n"},
            changelog="# Changelog\n\n## [Unreleased]\n\n### Fixed\n- stray\n\n## [0.1.0] - 2026-01-01\n",
        )
        result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("bullet(s) still under ## [Unreleased]", result.stderr)
        self.assertNotIn("all well formed", result.stdout)


if __name__ == "__main__":
    unittest.main()
