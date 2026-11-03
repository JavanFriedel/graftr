import copy
import json
import os
from pathlib import Path
import shutil

from test_cli import NativeCase
from status_fixture import write_record
import bench


class StorageTests(NativeCase):
    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def create(self, command):
        result = self.invoke(command, ["create", "bench", "sample", "--no-herdr", "--json"])
        self.assertEqual(result[0], 0, result)

    def freeze(self, command, *args):
        result = self.invoke(command, ["freeze", "bench--sample", "--yes", *args, "--json"])
        self.assertEqual(result[0], 0, result)
        return result

    def record_path(self):
        return self.fixture.state / "frozen/bench--sample.json"

    def test_freeze_and_thaw_regenerate_current_environment_and_retain_branch_and_note(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.create(command)
            frozen = self.freeze(command, "--note", "Return after upstream review 👩‍💻")
            self.assertFalse(self.fixture.checkout.exists())
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)
            self.assertEqual(self.record_path().stat().st_mode & 0o777, 0o600)
            self.assertEqual(self.record_path().parent.stat().st_mode & 0o777, 0o700)
            (self.fixture.repo / ".env").write_text("SECRET=synthetic-refreshed\n")
            thawed = self.invoke(command, ["thaw", "bench--sample", "--no-herdr", "--json"])
            self.assertEqual(thawed[0], 0, thawed)
            self.assertEqual(thawed[1]["data"]["status"], "thawed")
            self.assertIn("SECRET=synthetic-refreshed", (self.fixture.checkout / ".env").read_text())
            self.assertEqual((self.fixture.checkout / "node_modules/install-count").read_text().strip(), "1")
            self.assertEqual(json.loads(self.record_path().read_text())["note"], "Return after upstream review 👩‍💻")
            repeated = self.invoke(command, ["thaw", "bench--sample", "--no-herdr", "--json"])
            results.append(bench.normalize([list(frozen), list(thawed), list(repeated)], self.fixture.root))
        self.assertEqual(results[0], results[1])
        self.compare(["status", "--json"])

    def test_frozen_identity_reserves_branch_and_checkout_until_explicit_thaw(self):
        self.create(self.reference)
        self.freeze(self.reference)
        for args in [["create", "bench", "sample", "--no-herdr", "--json"], ["create", "bench", "other", "--branch", "task/sample", "--no-herdr", "--json"]]:
            expected = self.invoke(self.reference, args)
            actual = self.invoke([str(self.native)], args)
            self.assertEqual(bench.normalize(list(actual), self.fixture.root), bench.normalize(list(expected), self.fixture.root))
            self.assertEqual(actual[0], 1)
        self.compare(["frozen", "--json"])
        self.compare(["schedule", "bench--sample", "--until", "2100-01-01T12:00Z", "--json"])

    def test_moved_branch_is_never_reset_or_automatically_replayed(self):
        self.create(self.reference)
        self.freeze(self.reference)
        tree = self.fixture.git("rev-parse", "main^{tree}").strip()
        commit = self.fixture.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit-tree", tree, "-p", self.fixture.commit, "-m", "moved").strip()
        self.fixture.git("update-ref", "refs/heads/task/sample", commit, self.fixture.commit)
        result = self.compare(["thaw", "bench--sample", "--no-herdr", "--json"])
        self.assertEqual(result[0], 1)
        self.assertIn("missing or moved", result[1]["error"]["message"])
        self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), commit)
        self.assertFalse(self.fixture.checkout.exists())

    def test_dirty_freeze_and_invalid_date_or_note_do_not_record_intent(self):
        self.create(self.reference)
        (self.fixture.checkout / "tracked.txt").write_text("unsaved\n")
        result = self.compare(["freeze", "bench--sample", "--yes", "--json"])
        self.assertEqual(result[1]["error"]["phase"], "preflight")
        self.assertFalse(self.record_path().exists())
        for date in ["tomorrow", "2100-02-30T12:00Z", "2100-01-01T24:00Z", "2100-01-01T12:60Z", "2100-01-01T12:00:60Z", "0000-01-01T12:00Z", "2000-01-01T12:00Z", "2100-01-01"]:
            result = self.compare(["freeze", "bench--sample", "--yes", "--until", date, "--json"])
            self.assertEqual(result[1]["error"]["phase"], "arguments")
        for memo in ["", " ", "two\nlines", "terminal\x1bcontrol", "line\u2028break", "😀" * 1001]:
            self.compare(["freeze", "bench--sample", "--yes", "--note", memo, "--json"])
        self.assertFalse(self.record_path().exists())

    def test_interrupted_freeze_can_finish_after_checkout_and_active_record_are_gone(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.create(command)
            self.freeze(command)
            record = json.loads(self.record_path().read_text())
            record.update(status="freezing", message="Interrupted after removal")
            write_record(self.record_path(), record)
            result = self.invoke(command, ["freeze", "bench--sample", "--json"])
            self.assertEqual(result[0], 0)
            self.assertEqual(result[1]["data"]["status"], "frozen")
            results.append(bench.normalize(list(result), self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_cancel_schedule_fences_old_workers_even_for_standalone_record(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.create(command)
            self.freeze(command)
            old = json.loads(self.record_path().read_text())["generation"]
            scheduled = self.invoke(command, ["schedule", "sample", "--project", "bench", "--cancel", "--json"])
            self.assertEqual(scheduled[0], 0)
            self.assertNotEqual(json.loads(self.record_path().read_text())["generation"], old)
            worker = self.invoke(command, ["_thaw", "bench--sample", old, str(self.fixture.root / "config/graftr/projects.json"), str(self.fixture.state)])
            self.assertEqual(worker[0], 0)
            self.assertEqual(json.loads(worker[1])["status"], "skipped")
            self.assertFalse(self.fixture.checkout.exists())
            results.append(bench.normalize([list(scheduled), [worker[0], json.loads(worker[1]), worker[2]]], self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_failed_thaw_retains_active_ownership_and_stops_automatic_setup_retries(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.create(command)
            self.freeze(command)
            config_path = self.fixture.root / "config/graftr/projects.json"
            config = json.loads(config_path.read_text())
            config["projects"]["bench"]["installCommand"] = ["sh", "-c", "printf private-installer-error >&2; exit 7"]
            config_path.write_text(json.dumps(config))
            result = self.invoke(command, ["thaw", "bench--sample", "--no-herdr", "--json"])
            self.assertEqual(result[0], 1)
            self.assertEqual(result[1]["data"]["status"], "attention")
            self.assertTrue((self.fixture.state / "bench--sample.json").exists())
            record = json.loads(self.record_path().read_text())
            worker = self.invoke(command, ["_thaw", "bench--sample", record["generation"], str(config_path), str(self.fixture.state)])
            self.assertEqual(json.loads(worker[1])["status"], "skipped")
            self.assertNotIn("private-installer-error", str(result))
            results.append(bench.normalize([list(result), [worker[0], json.loads(worker[1]), worker[2]]], self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_welcome_message_checks_generation_and_uses_saved_worker_paths(self):
        self.create(self.reference)
        self.freeze(self.reference, "--note", "Read the review first")
        record = json.loads(self.record_path().read_text())
        args = ["_thaw-message", "bench--sample", record["generation"], str(self.fixture.root / "config/graftr/projects.json"), str(self.fixture.state)]
        self.compare(args, XDG_STATE_HOME="/irrelevant/saved-worker-paths")
        args[2] = "00000000-0000-0000-0000-000000000000"
        self.compare(args)

    def test_native_full_standalone_lifecycle_needs_no_bun_python_or_external_flock(self):
        command = [str(self.native)]
        runtime_path = self.fixture.root / "native-runtime-bin"
        runtime_path.mkdir(mode=0o700)
        for executable in ["git", "sh", "mkdir"]:
            (runtime_path / executable).symlink_to(shutil.which(executable))
        env = {"PATH": str(runtime_path)}
        for executable in ["bun", "python", "python3", "flock"]:
            self.assertIsNone(shutil.which(executable, path=env["PATH"]))
        for args in [["create", "bench", "sample", "--no-herdr"], ["open", "bench--sample", "--no-herdr"],
                     ["cleanup", "bench--sample"], ["close", "bench--sample"],
                     ["freeze", "bench--sample", "--yes"], ["thaw", "bench--sample", "--no-herdr"],
                     ["remove", "bench--sample", "--yes"]]:
            result = self.invoke(command, [*args, "--json"], **env)
            self.assertEqual(result[0], 0, result)
        self.assertFalse(self.fixture.checkout.exists())
        self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)
