import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import patch


ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("package_release", ROOT / "scripts/package-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="graftr-release-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for name in ["LICENSE", "README.md", "completions/graftr.bash", "plugins/herdr/herdr-plugin.toml"]:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fixture public source\n")
        self.binary = self.root / "graftr"
        self.binary.write_bytes(b"\x7fELF\x02\x01" + b"\0" * 12 + b"\x3e\x00" + b"fixture")
        dependency = self.root / "dependency"
        dependency.mkdir()
        (dependency / "LICENSE").write_text("fixture dependency license")
        self.metadata = {"resolve": {"root": "graftr"}, "packages": [
            {"id": "graftr", "name": "graftr", "version": "0.1.0"},
            {"id": "dep", "name": "dep", "version": "1.0.0", "license": "MIT", "license_file": None,
             "manifest_path": str(dependency / "Cargo.toml")} ]}
        self.rust = self.root / "rust-notices"
        (self.rust / "licenses").mkdir(parents=True)
        (self.rust / "COPYRIGHT-library.html").write_text("fixture Rust library notices")
        (self.rust / "licenses/MIT.txt").write_text("fixture Rust license")

    def package(self, destination="release", tag="v0.1.0"):
        return release.package(self.root, self.binary, self.metadata, self.rust, self.root / destination,
                               tag, release.TARGET, "a" * 40, 1767225600)

    def test_archive_is_deterministic_checksummed_and_includes_notices(self):
        first, second = self.package(), self.package("other")
        self.assertEqual(first.read_bytes(), second.read_bytes())
        digest = hashlib.sha256(first.read_bytes()).hexdigest()
        self.assertEqual((first.parent / "SHA256SUMS").read_text(), f"{digest}  {first.name}\n")
        with tarfile.open(first) as archive:
            members = archive.getmembers()
            self.assertTrue(all(member.isfile() for member in members))
            prefix = first.name.removesuffix(".tar.gz") + "/"
            receipt = json.load(archive.extractfile(prefix + "RELEASE.json"))
            self.assertEqual(receipt["version"], "0.1.0")
            self.assertIn("licenses/crates/dep-1.0.0/LICENSE", receipt["files"])
            self.assertIn("licenses/rust/COPYRIGHT-library.html", receipt["files"])
            self.assertEqual(archive.getmember(prefix + "graftr").mode, 0o755)
            for name, expected in receipt["files"].items():
                self.assertEqual(hashlib.sha256(archive.extractfile(prefix + name).read()).hexdigest(), expected)

    def test_existing_assets_are_not_overwritten(self):
        archive = self.package()
        previous = archive.read_bytes()
        with self.assertRaisesRegex(ValueError, "overwrite"):
            self.package()
        self.assertEqual(archive.read_bytes(), previous)

    def test_release_stamps_and_checks_the_executable_before_packaging(self):
        revision = "a" * 40
        def output(args, **kwargs):
            if args[:2] == ["git", "status"]:
                return b""
            if args[:2] == ["git", "rev-parse"]:
                return revision + "\n"
            if args[:2] == ["cargo", "metadata"]:
                return json.dumps(self.metadata).encode()
            if args[-2:] == ["--version", "--json"]:
                return json.dumps({"outcome": "completed", "data": reported}).encode()
            if args[-2:] == ["--print", "sysroot"]:
                return str(self.root) + "\n"
            if args[:2] == ["git", "show"]:
                return "1767225600\n"
            self.fail(f"Unexpected release command: {args}")
        reported = {"version": "0.1.0", "sourceRevision": revision}
        with patch.object(release, "ROOT", self.root), patch.object(sys, "argv", ["package-release.py", "--tag", "v0.1.0"]), \
             patch.object(release.subprocess, "check_output", side_effect=output), \
             patch.object(release.subprocess, "run") as build, patch.object(release, "package") as package, \
             patch("builtins.print"), patch.dict(os.environ, {"GRAFTR_SOURCE_REVISION": "b" * 40}):
            release.main()
            self.assertEqual(build.call_args.kwargs["env"]["GRAFTR_SOURCE_REVISION"], revision)
            package.assert_called_once()
            package.reset_mock()
            for reported in [{"version": "9.0.0", "sourceRevision": revision},
                             {"version": "0.1.0", "sourceRevision": None},
                             {"version": "0.1.0", "sourceRevision": "b" * 40}]:
                with self.assertRaisesRegex(ValueError, "version/source revision"):
                    release.main()
                package.assert_not_called()

    def test_invalid_version_binary_or_missing_license_blocks_release(self):
        with self.assertRaisesRegex(ValueError, "version"):
            self.package(tag="v9.0.0")
        self.binary.write_bytes(b"not an ELF executable")
        with self.assertRaisesRegex(ValueError, "ELF"):
            self.package()
        self.binary.write_bytes(b"\x7fELF\x02\x01" + b"\0" * 12 + b"\x3e\x00")
        (self.root / "dependency/LICENSE").unlink()
        with self.assertRaisesRegex(ValueError, "license text"):
            self.package()
        self.assertFalse((self.root / "release").exists())
