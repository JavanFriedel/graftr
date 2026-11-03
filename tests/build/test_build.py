import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


PROJECT = Path(__file__).resolve().parents[2]


class BuildTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="graftr-build-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        shutil.copyfile(PROJECT / "build.sh", self.root / "build.sh")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.cache = self.root / "custom cache"
        self.artifact = self.cache / "fixture-host/release/graftr"
        self.artifact.parent.mkdir(parents=True)
        self.artifact.write_text("#!/bin/sh\nexit 0\n")
        self.outfile = self.root / "dist/graftr"
        self.outfile.parent.mkdir()
        self.outfile.write_text("previous executable")
        for name, text in [("cargo", '#!/bin/sh\nprintf "%s\\n" "$@" > "$HOME/cargo-args"\n'),
                           ("rustc", "#!/bin/sh\nprintf 'rustc fixture\\nhost: fixture-host\\n'\n")]:
            path = self.bin / name
            path.write_text(text)
            path.chmod(0o700)
        self.env = {"HOME": str(self.root), "PATH": f"{self.bin}:/usr/bin:/bin", "CARGO_TARGET_DIR": "custom cache"}

    def invoke(self):
        return subprocess.run(["/bin/sh", str(self.root / "build.sh")], cwd=self.root, env=self.env,
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)

    def test_publication_replaces_inode_and_preserves_existing_hardlink(self):
        old = self.root / "old-inode"
        os.link(self.outfile, old)
        result = self.invoke()
        self.assertEqual((result.returncode, result.stdout, result.stderr), (0, f"{self.outfile}\n", ""))
        self.assertEqual(self.outfile.read_bytes(), self.artifact.read_bytes())
        self.assertEqual(old.read_text(), "previous executable")
        self.assertEqual(self.outfile.stat().st_mode & 0o777, 0o755)
        self.assertEqual(list(self.outfile.parent.iterdir()), [self.outfile])

    def test_smoke_or_missing_artifact_failure_preserves_previous_installation(self):
        self.artifact.write_text("#!/bin/sh\nexit 7\n")
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        self.assertIn("previous installation kept", result.stderr)
        self.assertEqual(self.outfile.read_text(), "previous executable")
        self.artifact.unlink()
        self.assertNotEqual(self.invoke().returncode, 0)
        self.assertEqual(self.outfile.read_text(), "previous executable")
        self.assertEqual(list(self.outfile.parent.iterdir()), [self.outfile])

    def test_host_target_and_custom_cache_are_explicit_and_build_failure_is_safe(self):
        self.assertEqual(self.invoke().returncode, 0)
        self.assertEqual((self.root / "cargo-args").read_text().splitlines(), [
            "build", "--manifest-path", str(self.root / "Cargo.toml"), "--release", "--locked", "--bin", "graftr",
            "--target", "fixture-host", "--target-dir", str(self.cache)])
        installed = self.outfile.read_bytes()
        (self.bin / "cargo").write_text("#!/bin/sh\nexit 7\n")
        result = self.invoke()
        self.assertEqual(result.returncode, 1)
        self.assertIn("previous installation kept", result.stderr)
        self.assertEqual(self.outfile.read_bytes(), installed)

    def test_empty_cache_or_invalid_compiler_host_never_publish(self):
        self.env["CARGO_TARGET_DIR"] = ""
        self.assertEqual(self.invoke().returncode, 1)
        self.env["CARGO_TARGET_DIR"] = "custom cache"
        (self.bin / "rustc").write_text("#!/bin/sh\nprintf 'host: invalid/path\\n'\n")
        self.assertEqual(self.invoke().returncode, 1)
        self.assertEqual(self.outfile.read_text(), "previous executable")


if __name__ == "__main__":
    unittest.main()
