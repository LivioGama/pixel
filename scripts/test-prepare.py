#!/usr/bin/env python3
"""Contract of .agents/skills/release/prepare.sh: the pull requests it lists.

Runs the real script inside a disposable workspace repository with stub
`cargo` and `gh` on PATH. The stub `gh` answers `pr list` with the pull
requests of the fixture, whatever the date search says (GitHub's own search
returned the previous prepare PR), and applies the script's `--jq` expression
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
    prs = json.loads(open(os.environ["FIXTURE_PRS"]).read())
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
        self.git("init", "-q", "-b", "develop")
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
        """A pull request merged into develop with a merge commit."""
        self.git("switch", "-q", "-c", branch)
        self.write(rel, branch + "\n")
        self.git("add", ".")
        self.git("commit", "-qm", branch)
        self.git("switch", "-q", "develop")
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

    def listed(self, stdout):
        head = "pull requests merged into develop since v0.1.0"
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
        self.write("CHANGELOG.md", "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- thing\n\n## [0.1.0] - 2026-01-01\n")
        self.git("commit", "-qam", "changelog")
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
        self.write("CHANGELOG.md", "# Changelog\n\n## [Unreleased]\n\n### Fixed\n- thing\n\n## [0.1.0] - 2026-01-01\n")
        self.git("commit", "-qam", "changelog")
        self.prs.write_text(json.dumps([
            {"number": 11, "title": "release: prepare 0.1.0", "mergeCommit": {"oid": released}},
        ]))

        result = self.prepare()

        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.listed(result.stdout), ["(none)"])


if __name__ == "__main__":
    unittest.main()
