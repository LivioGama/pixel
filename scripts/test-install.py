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
        (fake / "curl").write_text("""#!/usr/bin/env python3
import os, sys, shutil, json
from pathlib import Path
args = sys.argv
url = next(arg for arg in args if arg.startswith('https:'))
root = Path(os.environ['FIXTURE_ROOT'])
if url.endswith('/latest'):
    print(json.dumps({'tag_name': 'v9.8.7'}))
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


if __name__ == "__main__":
    unittest.main()
