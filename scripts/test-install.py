#!/usr/bin/env python3
"""Exercise the real installer with disposable homes and local release substitutes."""

import hashlib
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest


class InstallContract(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pixel-install-contract-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        fake = self.root / "fake"
        fake.mkdir()
        stage = self.root / "pixel-v9.8.7-aarch64-apple-darwin"
        (stage / "bin").mkdir(parents=True)
        binary = stage / "bin/pixel"
        binary.write_text("#!/bin/sh\necho isolated-pixel-9.8.7\n")
        binary.chmod(0o755)
        archive = self.root / "fixture.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            tar.add(stage, arcname=stage.name)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        (self.root / "sha").write_text(digest + "  fixture.tar.gz\n")
        (fake / "uname").write_text(
            "#!/bin/sh\ncase $1 in -s) echo Darwin;; -m) echo arm64;; esac\n"
        )
        # The fake answers like github.com: releases/latest redirects to the
        # tag page (FIXTURE_LATEST=tag), to the releases list when there is no
        # release (none), or the host cannot be reached (unreachable). The
        # anonymous REST API answers 403, as it does once a shared IP has used
        # its 60 requests an hour. Every URL asked is appended to curl.log.
        (fake / "curl").write_text("""#!/usr/bin/env python3
import os, sys, shutil
from pathlib import Path
args = sys.argv
url = next(arg for arg in args if arg.startswith('https:'))
root = Path(os.environ['FIXTURE_ROOT'])
with open(root / 'curl.log', 'a') as log:
    log.write(url + '\\n')
if url.startswith('https://api.github.com/'):
    sys.stderr.write('curl: (22) The requested URL returned error: 403\\n')
    sys.exit(22)
if url.endswith('/releases/latest'):
    mode = os.environ.get('FIXTURE_LATEST', 'tag')
    if mode == 'unreachable':
        sys.stderr.write('curl: (6) Could not resolve host: github.com\\n')
        sys.exit(6)
    page = 'releases/tag/v9.8.7' if mode == 'tag' else 'releases'
    sys.stdout.write('https://github.com/LivioGama/pixel/' + page)
elif url.endswith('.sha256'):
    print((root / 'sha').read_text())
else:
    shutil.copyfile(root / 'fixture.tar.gz', args[args.index('-o') + 1])
""")
        for executable in fake.iterdir():
            executable.chmod(0o755)
        self.destination = self.root / "destination"
        self.env = {
            **os.environ,
            "PATH": str(fake) + os.pathsep + os.environ["PATH"],
            "FIXTURE_ROOT": str(self.root),
            "PIXEL_INSTALL_DIR": str(self.destination),
            "HOME": str(self.root / "home"),
        }

    def install(self):
        return subprocess.run(
            ["sh", str(Path(__file__).with_name("install.sh"))],
            env=self.env, capture_output=True, text=True, timeout=15,
        )

    def test_install_and_reinstall_real_archive(self):
        for _ in range(2):
            result = self.install()
            self.assertEqual(result.returncode, 0, result.stderr)
            installed = subprocess.check_output(
                [str(self.destination / "pixel")], text=True, timeout=5
            )
            self.assertEqual(installed.strip(), "isolated-pixel-9.8.7")
            self.assertEqual(list(self.destination.glob(".pixel.tmp.*")), [])

    def test_checksum_failure_preserves_previous_install(self):
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        (self.root / "sha").write_text("0" * 64 + "  fixture.tar.gz\n")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Checksum mismatch!", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")
        self.assertEqual(list(self.destination.glob(".pixel.tmp.*")), [])

    def requested_urls(self):
        log = self.root / "curl.log"
        return log.read_text().splitlines() if log.exists() else []

    def test_latest_tag_comes_from_the_release_page_redirect_not_the_rate_limited_api(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        urls = self.requested_urls()
        self.assertEqual(
            [url for url in urls if "api.github.com" in url], [],
            "the anonymous API is rate limited per IP",
        )
        self.assertIn(
            "https://github.com/LivioGama/pixel/releases/download/v9.8.7/"
            "pixel-v9.8.7-aarch64-apple-darwin.tar.gz",
            urls,
        )
        self.assertIn("pixel v9.8.7 (aarch64-apple-darwin)", result.stdout)

    def test_a_repository_without_release_points_to_a_source_install(self):
        self.env["FIXTURE_LATEST"] = "none"
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No prebuilt release found for LivioGama/pixel.", result.stderr)
        self.assertIn("cargo install --git", result.stderr)
        self.assertFalse((self.destination / "pixel").exists())

    def test_an_unreachable_github_shows_the_curl_error_and_keeps_the_install(self):
        self.env["FIXTURE_LATEST"] = "unreachable"
        self.destination.mkdir()
        installed = self.destination / "pixel"
        installed.write_bytes(b"previous installation")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Could not resolve host: github.com", result.stderr)
        self.assertIn("Could not reach https://github.com/LivioGama/pixel/releases/latest", result.stderr)
        self.assertNotIn("No prebuilt release found", result.stderr)
        self.assertEqual(installed.read_bytes(), b"previous installation")


if __name__ == "__main__":
    unittest.main()
