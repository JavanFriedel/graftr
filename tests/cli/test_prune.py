import hashlib
import json
import os
from pathlib import Path
import shutil

from test_cli import NativeCase
from status_fixture import write_record, plan


OLD = 1767225600.123


class PruneTests(NativeCase):
    def retained(self):
        self.fixture.state.mkdir(parents=True, mode=0o700, exist_ok=True)
        for category in ["removed", "timings", "env-backups", "migration-backups", "backups", "creating", "hook-work"]:
            directory = self.fixture.state / category
            directory.mkdir(mode=0o700)
            old = directory / "old"
            old.write_bytes(b"x" * 1280)
            old.chmod(0o600)
            os.utime(old, (OLD, OLD))
            (directory / "new").write_text("keep recent")
        for category, values in [("tailscale-hooks", [{"route": "active"}, {"route": "none"}, {"route": "pending"}]),
                                 ("postgres-hooks", [{"state": "active"}, {"state": "dropped"}])]:
            (self.fixture.state / category).mkdir(mode=0o700)
            for index, value in enumerate(values):
                write_record(self.fixture.state / category / f"entry-{index}" / "receipt.json", value)
        corrupt = self.fixture.state / "postgres-hooks/corrupt"
        corrupt.mkdir(mode=0o700)
        (corrupt / "receipt.json").write_text("not JSON; synthetic-private")
        for entry in [self.fixture.state / "tailscale-hooks", self.fixture.state / "postgres-hooks"]:
            for item in sorted(entry.rglob("*"), reverse=True):
                os.utime(item, (OLD, OLD))
        log = self.fixture.state / "logs/orphan/old.log"
        (self.fixture.state / "logs").mkdir(mode=0o700)
        log.parent.mkdir(parents=True, mode=0o700)
        log.write_text("old private log")
        os.utime(log, (OLD, OLD))
        outside = self.fixture.root / "outside"
        outside.mkdir()
        (outside / "untouched").write_text("outside")
        nested = self.fixture.state / "removed/nested"
        nested.mkdir()
        (nested / "link").symlink_to(outside, target_is_directory=True)
        os.utime(nested / "link", (OLD, OLD), follow_symlinks=False)
        os.utime(nested, (OLD, OLD))
        (self.fixture.state / "removed/ignored-symlink").symlink_to(outside)
        (self.fixture.state / "unknown-entry").write_text("unknown")

    def test_preview_reports_only_old_disposable_entries_and_preserves_resource_evidence(self):
        self.retained()
        result = self.compare(["prune", "--json"])
        self.assertEqual(result[0], 0)
        self.assertEqual(result[1]["data"]["status"], "preview")
        kept = result[1]["data"]["kept"]
        self.assertEqual(len(kept), 4)
        self.assertNotIn("synthetic-private", str(result))
        self.assertEqual(result[1]["data"]["unrecognized"], ["unknown-entry"])
        for columns in [24, 40, 80, 120]:
            self.compare(["prune"], COLUMNS=str(columns))
        self.compare(["prune", "--older-than", "3650", "--json"])

    def test_apply_matches_source_and_does_not_follow_links_or_delete_resource_receipts(self):
        self.retained()
        backup = self.fixture.root / "state-copy"
        shutil.copytree(self.fixture.state, backup, symlinks=True)
        expected = self.invoke(self.reference, ["prune", "--apply", "--json"])
        expected_files = sorted(str(p.relative_to(self.fixture.state)) for p in self.fixture.state.rglob("*"))
        shutil.rmtree(self.fixture.state)
        shutil.copytree(backup, self.fixture.state, symlinks=True)
        actual = self.invoke([str(self.native)], ["prune", "--apply", "--json"])
        self.assertEqual(actual, expected)
        self.assertEqual(actual[0], 0)
        self.assertEqual(actual[1]["data"]["status"], "applied")
        self.assertEqual(sorted(str(p.relative_to(self.fixture.state)) for p in self.fixture.state.rglob("*")), expected_files)
        self.assertEqual((self.fixture.root / "outside/untouched").read_text(), "outside")
        self.assertTrue((self.fixture.state / "removed/ignored-symlink").is_symlink())
        self.assertTrue((self.fixture.state / "tailscale-hooks/entry-0/receipt.json").exists())
        self.compare(["prune", "--json"])

    def test_live_cleanup_and_retirement_scratch_is_kept(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        record_path = self.fixture.state / "bench--sample.json"
        task = json.loads(record_path.read_text())
        task["cleanup"] = plan("cleanup")
        task["retire"] = plan("retire")
        write_record(record_path, task)
        key = hashlib.sha256(json.dumps([task["id"], task["cleanup"]["runId"]], separators=(",", ":")).encode()).hexdigest()
        for category in ["hook-work", "tailscale-hooks", "postgres-hooks"]:
            (self.fixture.state / category).mkdir(mode=0o700)
            directory = self.fixture.state / category / key
            directory.mkdir(parents=True, mode=0o700)
            os.utime(directory, (OLD, OLD))
        result = self.compare(["prune", "--json"])
        self.assertEqual(len(result[1]["data"]["kept"]), 3)
        self.assertTrue(all(item["reason"] == "a current task still uses it" for item in result[1]["data"]["kept"]))

    def test_invalid_days_and_public_retained_roots_refuse_without_deleting(self):
        self.retained()
        for days in ["0", "3651", "-1", "1.5", "Infinity", "NaN", "", "$(touch PWN)"]:
            self.compare(["prune", "--older-than=" + days, "--json"])
        (self.fixture.state / "removed").chmod(0o755)
        (self.fixture.state / "operation.lock").touch(mode=0o600)
        self.compare(["prune", "--apply", "--json"])
