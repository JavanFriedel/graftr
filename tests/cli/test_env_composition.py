import json
from pathlib import Path
import re

from test_cli import NativeCase
import bench


class EnvironmentCompositionTests(NativeCase):
    def create(self):
        return self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"])

    def definitions(self, contents, key):
        return re.findall(rb"(?m)^" + key.encode() + rb"=([^\r\n]*)", contents.removeprefix(b"\xef\xbb\xbf"))

    def test_fresh_copy_has_unique_keys_and_preserves_raw_unmanaged_syntax(self):
        baseline = self.fixture.repo / ".env"
        untouched = ("# private baseline\r\nSECRET='synthetic-secret # literal $value' # retained\r\n"
                     'MULTILINE="first\r\nPORT=not-an-assignment\r\nlast"\r\n'
                     "REFERENCE=${PORT}/callback\r\n").encode()
        baseline.write_bytes(b"\xef\xbb\xbfPORT=3000\r\nPORT=3001\r\n" + untouched + b"DUPLICATE=old\r\nDUPLICATE='kept' # last definition\r\n")
        original = baseline.read_bytes()
        result = self.create()
        self.assertEqual(result[0], 0, result)
        target = self.fixture.checkout / ".env"
        contents = target.read_bytes()
        port = json.loads((self.fixture.state / "bench--sample.json").read_text())["localPort"]
        # The apparent second match is literal text inside MULTILINE, not a key.
        definitions = self.definitions(contents, "PORT")
        self.assertEqual(definitions, [str(port).encode(), b"not-an-assignment"])
        self.assertEqual(contents.count(b"\r\nPORT="), 1)  # Only literal text inside MULTILINE remains.
        self.assertIn(untouched, contents)
        self.assertTrue(contents.startswith(b"\xef\xbb\xbfPORT=" + str(port).encode() + b"\r\n"))
        self.assertEqual(self.definitions(contents, "DUPLICATE"), [b"'kept' # last definition"])
        self.assertLess(contents.index(b"PORT=" + str(port).encode()), contents.index(b"REFERENCE="))
        self.assertEqual(baseline.read_bytes(), original)
        self.assertEqual(target.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("synthetic-secret", json.dumps(result))

    def test_managed_multiline_assignments_are_replaced_as_whole_statements_without_execution(self):
        baseline = self.fixture.repo / ".env"
        baseline.write_text('PORT="old\nPRIVATE_FRAGMENT=must-disappear\nend" # old assignment\n'
                            "PORT=second\nSECRET='$(touch NEVER_RUN)'\nEMPTY=\n")
        result = self.create()
        self.assertEqual(result[0], 0, result)
        contents = (self.fixture.checkout / ".env").read_bytes()
        port = json.loads((self.fixture.state / "bench--sample.json").read_text())["localPort"]
        self.assertEqual(self.definitions(contents, "PORT"), [str(port).encode()])
        self.assertNotIn(b"PRIVATE_FRAGMENT", contents)
        self.assertNotIn(b"end\"", contents)
        self.assertIn(b"SECRET='$(touch NEVER_RUN)'\nEMPTY=\n", contents)
        self.assertFalse((self.fixture.checkout / "NEVER_RUN").exists())
        self.assertNotIn("PRIVATE_FRAGMENT", json.dumps(result))

    def test_existing_environment_is_not_rewritten_by_repeated_setup(self):
        self.assertEqual(self.create()[0], 0)
        target = self.fixture.checkout / ".env"
        original = target.read_bytes() + b"\nLOCAL_ONLY='synthetic-local' # preserve me\n"
        target.write_bytes(original)
        inode = target.stat().st_ino
        (self.fixture.repo / ".env").write_text("SECRET=synthetic-new-baseline\nPORT=1234\n")
        result = self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=self.fixture.checkout)
        self.assertEqual(result[0], 0, result)
        self.assertEqual((target.read_bytes(), target.stat().st_ino), (original, inode))

    def test_selected_refresh_replaces_all_occurrences_and_keeps_other_bytes_and_private_backup(self):
        self.assertEqual(self.create()[0], 0)
        baseline = self.fixture.repo / ".env"
        baseline.chmod(0o600)
        baseline.write_text("SECRET='synthetic-first'\nSECRET='synthetic-new' # chosen baseline\nADDED=present\n")
        target = self.fixture.checkout / ".env"
        port = json.loads((self.fixture.state / "bench--sample.json").read_text())["localPort"]
        prefix = f"\ufeff# local\r\nPORT={port}\r\n".encode()
        kept = b"LOCAL_ONLY='synthetic-local' # unchanged\r\nUNSELECTED=first\r\nUNSELECTED=last\r\n"
        original = prefix + b"SECRET=synthetic-old\r\n" + kept + b"SECRET='synthetic-shadow'\r\n"
        target.write_bytes(original)
        before = bench.readonly_snapshot(self.fixture)
        args = ["env", "refresh", "bench--sample", "--take-baseline", "SECRET", "--json"]
        preview = self.invoke([str(self.native)], args)
        self.assertEqual(preview[0], 0, preview)
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        result = self.invoke([str(self.native)], [*args, "--apply"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["data"]["status"], "applied")
        contents = target.read_bytes()
        self.assertTrue(contents.startswith(prefix))
        self.assertIn(kept, contents)
        self.assertEqual(self.definitions(contents, "SECRET"), [b"'synthetic-new' # chosen baseline"])
        self.assertEqual(self.definitions(contents, "ADDED"), [b"present"])
        self.assertNotIn(b"synthetic-old", contents)
        self.assertNotIn(b"synthetic-shadow", contents)
        backup = Path(result[1]["data"]["backupPath"])
        self.assertEqual(backup.read_bytes(), original)
        self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
        self.assertEqual(target.stat().st_mode & 0o777, 0o600)
        self.assertNotIn("synthetic-", json.dumps(result))
        again = self.invoke([str(self.native)], [*args, "--apply"])
        self.assertEqual(again[1]["data"]["status"], "unchanged")
        self.assertIsNone(again[1]["data"]["backupPath"])
        self.assertEqual(target.read_bytes(), contents)

    def test_explicit_selection_normalizes_duplicates_even_when_last_value_already_matches(self):
        self.assertEqual(self.create()[0], 0)
        baseline = self.fixture.repo / ".env"
        baseline.chmod(0o600)
        baseline.write_text("SECRET=synthetic-current\n")
        target = self.fixture.checkout / ".env"
        with target.open("ab") as stream:
            stream.write(b"SECRET=synthetic-stale\nSECRET=synthetic-current\n")
        args = ["env", "refresh", "bench--sample", "--take-baseline", "SECRET", "--json"]
        preview = self.invoke([str(self.native)], args)
        entry = next(entry for entry in preview[1]["data"]["entries"] if entry["key"] == "SECRET")
        self.assertEqual((entry["status"], entry["action"]), ("same", "replace"))
        result = self.invoke([str(self.native)], [*args, "--apply"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual(self.definitions(target.read_bytes(), "SECRET"), [b"synthetic-current"])

    def test_invalid_baseline_leaves_owned_checkout_without_partial_environment_or_secret_output(self):
        baseline = self.fixture.repo / ".env"
        baseline.write_text('SECRET="synthetic-private-unclosed\nPORT=1234\n')
        result = self.create()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["data"]["status"], "incomplete")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse((self.fixture.checkout / ".env").exists())
        self.assertFalse((self.fixture.checkout / "node_modules").exists())
        self.assertTrue((self.fixture.state / "bench--sample.json").exists())
        self.assertNotIn("synthetic-private-unclosed", json.dumps(result))
