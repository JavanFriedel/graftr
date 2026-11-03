import copy
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess

from test_cli import NativeCase
from status_fixture import write_record
import bench


class EnvironmentTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        record = self.fixture.state / "bench--sample.json"
        task = json.loads(record.read_text())
        config_file = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(config_file.read_text())
        baseline = self.fixture.repo / ".env"
        config["projects"]["bench"]["envSource"] = str(baseline)
        config_file.write_text(json.dumps(config))
        baseline.write_text("SECRET=synthetic-rotated\nKEPT=same\nADDED='synthetic-new' # retained\n")
        baseline.chmod(0o600)
        target = Path(task["path"]) / ".env"
        target.write_bytes(f'# original\r\nPORT={task["localPort"]}\r\nSECRET=synthetic-original\r\nKEPT=same\r\nLOCAL_ONLY=keep\r\n'.encode())
        target.chmod(0o600)
        return task, record, baseline, target

    def test_preview_is_readonly_and_values_hidden_with_selective_replacement(self):
        task, _, _, _ = self.managed()
        for args in [["env", "refresh", task["id"], "--json"],
                     ["env", "refresh", "sample", "--project", "bench", "--take-baseline", "SECRET", "--json"],
                     ["env", "refresh", "--json"]]:
            result = self.compare(args, cwd=Path(task["path"]))
            self.assertEqual(result[0], 0)
            self.assertNotIn("synthetic-", str(result))
        for width in [24, 40, 80, 120]:
            self.compare(["env", "refresh", task["id"]], COLUMNS=str(width))
        for args in [["env", "refresh", "--json"], ["env", "--json"], ["env", "wat", "--json"]]:
            self.compare(args, cwd=self.fixture.root)

    def test_dotenv_syntax_multiline_escapes_duplicates_and_raw_copying(self):
        task, _, baseline, target = self.managed()
        documents = [
            "\ufeff# BOM\r\nexport EXPORTED = literal\r\nCOLON: literal\r\nSECRET='first'\r\nSECRET=\"last\" # final\r\nMULTILINE=\"one\r\ntwo\"\r\nREFERENCE=${PORT}/callback\r\n",
            'DOUBLE="a\\nb\\rc\\td"\nSINGLE=\'a\\\'b\'\nBACKTICK=`a\\`b`\nPLAIN=one  two # comment\nEMPTY=\n',
            'UNICODE="日本語 👩‍👩‍👧‍👦"\nSECRET="$(touch NEVER_RUN)"\n__proto__=literal\n',
        ]
        for document in documents:
            baseline.write_bytes(document.encode())
            result = self.compare(["env", "refresh", task["id"], "--json"])
            self.assertEqual(result[0], 0)
        # Matching values with different quoting remain unchanged.
        baseline.write_text("SECRET='synthetic-original'\nKEPT=\"same\"\n")
        self.compare(["env", "refresh", task["id"], "--take-baseline=SECRET", "--json"])
        self.assertFalse((Path(task["path"]) / "NEVER_RUN").exists())

    def test_malformed_binary_oversized_and_managed_drift_are_rejected_privately(self):
        task, _, baseline, target = self.managed()
        invalid = [b'SECRET="unterminated\n', b"not an assignment\n", b"SECRET='value' unexpected\n", b"BAD.KEY=value\n",
                   b"=value\n", b"export BAD\n", b"SECRET=one\0two\n", b"\xff\xfe", b" " * (4 * 1024 * 1024 + 1)]
        for file in [baseline, target]:
            original = file.read_bytes()
            for document in invalid:
                file.write_bytes(document)
                result = self.compare(["env", "refresh", task["id"], "--json"])
                self.assertEqual(result[0], 1)
                self.assertNotIn("unterminated", str(result))
            file.write_bytes(original)
        target.write_text("PORT=private-drift\n")
        result = self.compare(["env", "refresh", task["id"], "--json"])
        self.assertEqual(result[1]["error"]["code"], "ENV_MANAGED_MISMATCH")
        self.assertNotIn("private-drift", str(result))

    def test_literal_repeated_selection_validation(self):
        task, _, _, _ = self.managed()
        for selected in [["SECRET", "SECRET"], ["PORT"], ["MISSING"], ["$(touch PWN)"], ["SECRET=private-value"]]:
            args = ["env", "refresh", task["id"], "--json"]
            for key in selected:
                args.extend(["--take-baseline", key])
            result = self.compare(args)
            self.assertEqual(result[0], 1)
        self.assertFalse((Path(task["path"]) / "PWN").exists())

    def test_apply_preserves_bytes_writes_independent_private_backup_and_is_idempotent(self):
        task, record, baseline, target = self.managed()
        original = target.read_bytes()
        records = record.read_bytes()
        results, contents = [], []
        for command in [self.reference, [str(self.native)]]:
            target.write_bytes(original)
            first = self.invoke(command, ["env", "refresh", task["id"], "--apply", "--json"])
            self.assertEqual(first[0], 0)
            report = first[1]["data"]
            self.assertEqual(report["status"], "applied")
            backup = Path(report["backupPath"])
            self.assertEqual(backup.read_bytes(), original)
            self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
            self.assertEqual(backup.stat().st_nlink, 1)
            self.assertNotEqual(backup.stat().st_ino, target.stat().st_ino)
            self.assertEqual(backup.parent.stat().st_mode & 0o777, 0o700)
            self.assertEqual(target.stat().st_mode & 0o777, 0o600)
            self.assertEqual(record.read_bytes(), records)
            self.assertTrue(target.read_bytes().startswith(original))
            contents.append(target.read_bytes())
            report["backupPath"] = "<private-backup>"
            results.append(first)
            before = list(backup.parent.glob("*.env"))
            second = self.invoke(command, ["env", "refresh", task["id"], "--apply", "--json"])
            self.assertEqual(second[1]["data"]["status"], "unchanged")
            self.assertIsNone(second[1]["data"]["backupPath"])
            self.assertEqual(list(backup.parent.glob("*.env")), before)
            selected = self.invoke(command, ["env", "refresh", task["id"], "--take-baseline=SECRET", "--apply", "--json"])
            self.assertEqual(selected[1]["data"]["status"], "applied")
            self.assertFalse(selected[1]["data"]["reviewRequired"])
            self.assertTrue(target.read_bytes().endswith(b"SECRET=synthetic-rotated\n"))
        self.assertEqual(results[0], results[1])
        self.assertEqual(contents[0], contents[1])

    def test_public_symlink_missing_files_and_pending_receipts_are_not_repaired(self):
        task, _, baseline, target = self.managed()
        for file in [baseline, target]:
            original = file.read_bytes()
            file.chmod(0o644)
            self.compare(["env", "refresh", task["id"], "--json"])
            file.chmod(0o600)
            file.unlink()
            self.compare(["env", "refresh", task["id"], "--json"])
            outside = self.fixture.root / "private-environment"
            outside.write_bytes(original)
            outside.chmod(0o600)
            file.symlink_to(outside)
            self.compare(["env", "refresh", task["id"], "--json"])
            file.unlink()
            file.write_bytes(original)
            file.chmod(0o600)
        receipt_file = next((self.fixture.state / "setup").glob("*.json"))
        receipt = json.loads(receipt_file.read_text())
        receipt.update(status="pending", step="dependencies", dependencies="pending")
        write_record(receipt_file, receipt)
        self.compare(["env", "refresh", task["id"], "--json"])

    def test_apply_lock_and_git_backup_boundaries(self):
        task, _, _, target = self.managed()
        original = target.read_bytes()
        lock = self.fixture.state / ("setup-" + hashlib.sha256(task["path"].encode()).hexdigest() + ".lock")
        with lock.open("w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.compare(["env", "refresh", task["id"], "--apply", "--json"])
        self.assertFalse((self.fixture.state / "env-backups").exists())
        backups = self.fixture.state / "env-backups"
        backups.mkdir(mode=0o755)
        result = self.compare(["env", "refresh", task["id"], "--apply", "--json"])
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertEqual(result[1]["progress"], [{"action": "backup-environment", "target": task["id"], "status": "uncertain"}])
        backups.chmod(0o700)
        subprocess.run(["git", "init", "-q", str(backups)], env=self.fixture.env, check=True)
        result = self.compare(["env", "refresh", task["id"], "--apply", "--json"])
        self.assertEqual(result[1]["error"]["code"], "ENV_BACKUP_LOCATION")
        self.assertEqual(target.read_bytes(), original)
        self.assertFalse(list(backups.glob("*.env")))

    def test_tracked_checkout_and_changed_configuration_are_refused(self):
        task, _, _, target = self.managed()
        subprocess.run(["git", "-C", task["path"], "add", "--force", ".env"], env=self.fixture.env, check=True)
        result = self.compare(["env", "refresh", task["id"], "--json"])
        self.assertEqual(result[1]["error"]["code"], "ENV_REFRESH_TRACKED")
        subprocess.run(["git", "-C", task["path"], "reset", "-q", "--", ".env"], env=self.fixture.env, check=True)
        config_file = self.fixture.root / "config/graftr/projects.json"
        original = json.loads(config_file.read_text())
        for changes in [{"envSource": str(self.fixture.repo / ".env.prod")}, {"envOverrides": {"PORT": "9999"}}, {"repo": str(self.fixture.root)}]:
            config = copy.deepcopy(original)
            config["projects"]["bench"].update(changes)
            config_file.write_text(json.dumps(config))
            self.compare(["env", "refresh", task["id"], "--json"])
