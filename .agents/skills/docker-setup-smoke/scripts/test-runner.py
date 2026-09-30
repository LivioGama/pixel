#!/usr/bin/env python3
"""Verify binary selection and failed-run evidence without starting Docker."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


class RunnerContract(unittest.TestCase):
    def setUp(self):
        self.scratch = tempfile.TemporaryDirectory()
        self.addCleanup(self.scratch.cleanup)
        self.root = Path(self.scratch.name)
        scripts = self.root / "repo/.agents/skills/docker-setup-smoke/scripts"
        scripts.mkdir(parents=True)
        self.runner = scripts / "run.sh"
        shutil.copyfile(Path(__file__).with_name("run.sh"), self.runner)
        tools = self.root / "bin"
        tools.mkdir()
        docker = tools / "docker"
        docker.write_text("""#!/usr/bin/env python3
import json, os, sys
with open(os.environ['CALL_LOG'], 'a') as log:
    log.write(json.dumps(sys.argv[1:]) + '\\n')
if sys.argv[1] == 'run':
    print('container output retained')
    sys.exit(int(os.environ.get('RUN_EXIT', '0')))
""")
        docker.chmod(0o755)
        git = tools / "git"
        git.write_text("#!/bin/sh\nprintf '%040d\\n' 1\n")
        git.chmod(0o755)
        self.log = self.root / "calls.jsonl"
        self.env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}", CALL_LOG=str(self.log))

    def run_case(self, *args, exit_code=0):
        result = subprocess.run(["bash", str(self.runner), *args], env=dict(self.env, RUN_EXIT=str(exit_code)),
                                capture_output=True, text=True, timeout=10)
        calls = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return result, calls

    def test_source_modes_build_the_requested_head_instead_of_a_release_or_merge_ref(self):
        sha = "a" * 40
        for args, ref in [(('--source', 'main'), 'refs/heads/main'),
                          (('--pr', '427'), 'refs/pull/427/head'), (('--source', sha), sha)]:
            with self.subTest(args=args):
                result, calls = self.run_case(*args)
                self.assertEqual(result.returncode, 0, result.stderr)
                run = [call for call in calls if call[0] == 'run'][-1]
                self.assertIn(f'PIXEL_SOURCE_REF={ref}', run)
                self.assertIn('PIXEL_BOOTSTRAP=source.sh', run)
                self.assertIn('PIXEL_BOOTSTRAP_TIMEOUT=1800', run)
                self.assertTrue(any(arg.startswith('rust:1.98.1-bookworm@sha256:') for arg in run))
                self.assertNotIn('refs/pull/427/merge', run)

    def test_release_mode_downloads_the_named_release(self):
        result, calls = self.run_case('v0.6.1')
        self.assertEqual(result.returncode, 0, result.stderr)
        run = next(call for call in calls if call[0] == 'run')
        self.assertIn('PIXEL_RELEASE=v0.6.1', run)
        self.assertIn('PIXEL_BOOTSTRAP=bootstrap.sh', run)
        self.assertIn('PIXEL_SOURCE_REF=', run)
        self.assertTrue(any(arg.startswith('debian:bookworm-slim@sha256:') for arg in run))

    def test_ambiguous_or_invalid_selectors_never_provision_a_container(self):
        for args in [('v0.6.1', '--source', 'main'), ('--pr', '0'), ('--source', 'bad'), ('--pr', '427;echo')]:
            with self.subTest(args=args):
                result, calls = self.run_case(*args)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(calls, [])

    def test_container_failure_keeps_its_status_and_exports_evidence_before_cleanup(self):
        result, calls = self.run_case('--pr', '427', exit_code=23)
        self.assertEqual(result.returncode, 23)
        self.assertLess(next(i for i, call in enumerate(calls) if call[0] == 'cp'),
                        next(i for i, call in enumerate(calls) if call[0] == 'rm'))
        evidence = next((self.root / 'repo/target/docker-setup-smoke').iterdir())
        self.assertEqual((evidence / 'exit-status.txt').read_text(), '23\n')
        self.assertIn('container output retained', (evidence / 'run.log').read_text())


if __name__ == '__main__':
    unittest.main()
