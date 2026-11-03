import copy
import json
from pathlib import Path
import sys

from test_cli import NativeCase

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "benchmarks"))
from status_fixture import seed_status, write_record
import bench


class StatusTests(NativeCase):
    def prepare(self, count=4):
        return seed_status(self.fixture, count)

    def test_empty_status_and_frozen_are_readonly(self):
        for args in [["status"], ["status", "--json"], ["frozen"], ["frozen", "--json"], ["frozen", "other", "--json"]]:
            self.compare(args)
        self.assertFalse(self.fixture.state.exists())

    def test_mixed_records_and_narrow_output_match(self):
        self.prepare()
        for columns in [1, 16, 24, 40, 60, 80, 120]:
            self.compare(["status"], COLUMNS=str(columns))
            self.compare(["frozen"], COLUMNS=str(columns))
        for args in [["status", "--json"], ["frozen", "--json"], ["frozen", "bench", "--json"], ["frozen", "other", "--json"]]:
            result = self.compare(args)
            self.assertNotIn("synthetic-private-argument", json.dumps(result))
            self.assertNotIn("synthetic-private-context", json.dumps(result))

    def test_task_schema_and_environment_corruption_is_refused(self):
        files = self.prepare()
        path = files["tasks"][0]
        original = json.loads(path.read_text())
        changes = [
            ("schemaVersion", 2), ("schemaVersion", 3), ("schemaVersion", 999), ("status", "pretend-ready"),
            ("id", "other--item-000"), ("localPort", 80), ("httpsPort", original["localPort"]),
            ("ports", {}), ("ports", {"bad--name": 3500}), ("ports", {"api": 80}),
            ("socket", None), ("url", "https://fixture.example.test:5200/secret"),
            ("host", "different.test"), ("envOverrides", {"PASSWORD": "not-public"}),
            ("envOverrides", {"PATH": "not-public"}), ("envOverrides", {"GIT_CONFIG": "not-public"}),
            ("envOverrides", {"PORT": "{{unknown}}"}), ("envOverrides", {"PORT": "{{{localPort}}}"}),
            ("envOverrides", {"PORT": "{{httpsPort}}"}), ("overrides", {"PORT": "synthetic-secret-marker$unsafe"}),
            ("overrides", {"PORT": "9999"}), ("envOverrides", []), ("tabs", {"agent": ""}),
        ]
        for key, value in changes:
            record = copy.deepcopy(original)
            record[key] = value
            if key == "socket":
                record["workspaceId"] = "foreign"
            write_record(path, record)
            self.assert_refused(["status", "--json"], partial=True)
        write_record(path, original)
        self.compare(["status", "--json"])

    def assert_refused(self, args, partial=False):
        before = bench.readonly_snapshot(self.fixture)
        if partial:
            actual = self.invoke([str(self.native)], args)
            self.assertEqual(actual[0], 1, actual)
            self.assertFalse(actual[1]["ok"])
            self.assertEqual(actual[1]["outcome"], "partial")
            self.assertEqual(actual[1]["error"]["code"], "RECORDS_INCOMPLETE")
            self.assertEqual(len(actual[1]["data"]["recordIssues"]), 1)
            self.assertNotIn("synthetic-secret-marker", json.dumps(actual))
            self.assertEqual(bench.readonly_snapshot(self.fixture), before)
            return
        expected = self.invoke(self.reference, args)
        actual = self.invoke([str(self.native)], args)
        self.assertEqual(expected[0], 1, expected)
        self.assertEqual(actual[0], 1, actual)
        for result in (expected, actual):
            self.assertFalse(result[1]["ok"])
            self.assertEqual(result[1]["outcome"], "failed")
            self.assertIsNone(result[1]["data"])
            self.assertNotIn("synthetic-secret-marker", json.dumps(result))
        self.assertEqual(actual[1]["error"]["code"], expected[1]["error"]["code"])
        self.assertEqual(actual[1]["error"]["phase"], expected[1]["error"]["phase"])
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)

    def test_hook_plan_and_receipt_corruption_is_refused(self):
        files = self.prepare()
        cases = [
            (files["tasks"][0], ["cleanup", "status"], "complete"),
            (files["tasks"][0], ["cleanup", "attempt"], -1),
            (files["tasks"][0], ["cleanup", "hooks", 0, "timeoutMs"], 1),
            (files["tasks"][0], ["cleanup", "hooks", 0, "command"], ["relative/executable"]),
            (files["tasks"][0], ["cleanup", "context"], {"LD_PRELOAD": "synthetic-secret-marker"}),
            (files["tasks"][0], ["retire", "results", 0, "phase"], "cleanup"),
            (files["tasks"][0], ["retire", "results", 0, "name"], "different-hook"),
            (files["tasks"][0], ["retire", "results", 0, "reason"], "unknown"),
            (files["setups"][0], ["status"], "bad"), (files["setups"][0], ["skipInstall"], "false"),
            (files["setups"][0], ["installOutputs", 0, "path"], "../private"),
            (files["setups"][0], ["installLog"], "relative.log"),
            (files["setups"][0], ["timings", "id"], "not-an-id"),
            (files["setups"][0], ["timings", "elapsedMs"], -1),
            (files["setups"][0], ["timings", "stagesMs"], {"unknown": 1}),
            (files["setups"][0], ["hooks", 0, "status"], "running"),
            (files["setups"][0], ["hooks", 0, "log"], "relative.log"),
        ]
        for path, keys, value in cases:
            original = json.loads(path.read_text())
            record = copy.deepcopy(original)
            node = record
            for key in keys[:-1]:
                node = node[key]
            node[keys[-1]] = value
            write_record(path, record)
            self.assert_refused(["status", "--json"], partial=True)
            write_record(path, original)

    def test_adoption_and_frozen_corruption_is_refused(self):
        files = self.prepare()
        cases = [("adopted", "schemaVersion", 2), ("adopted", "localPort", "3001"),
                 ("adopted", "path", str(self.fixture.root / "external/../unsafe")),
                 ("adopted", "socket", "foreign"), ("adopted", "environment", "ready"),
                 ("adopted", "url", "https://user:password@example.test"),
                 ("adopted", "adoptedAt", "not-a-date"), ("adopted", "hooks", [{"phase": "retire", "name": "test", "status": "failed"}]),
                 ("frozen", "schemaVersion", 2), ("frozen", "generation", "old-generation"),
                 ("frozen", "status", "ready"), ("frozen", "frozenAt", "2026-01-01T00:00:00Z"),
                 ("frozen", "attempts", -1), ("frozen", "note", "unsafe\nmessage"), ("frozen", "skipInstall", "false")]
        for kind, key, value in cases:
            path = files[kind][0]
            original = json.loads(path.read_text())
            record = copy.deepcopy(original)
            record[key] = value
            write_record(path, record)
            self.assert_refused(["status", "--json"], partial=True)
            if kind == "frozen":
                self.assert_refused(["frozen", "other", "--json"])
            write_record(path, original)

    def test_private_files_directories_and_record_filename_bindings(self):
        files = self.prepare()
        for kind in files:
            path = files[kind][0]
            path.chmod(0o644)
            self.assert_refused(["status", "--json"], partial=True)
            path.chmod(0o600)
            moved = path.with_name("moved-original")
            path.rename(moved)
            path.symlink_to(moved.name)
            self.assert_refused(["status", "--json"], partial=True)
            path.unlink()
            moved.rename(path)
            wrong = path.with_name("bench--wrong.json")
            path.rename(wrong)
            self.assert_refused(["status", "--json"], partial=True)
            wrong.rename(path)
        for name in ("setup", "adopted", "frozen"):
            directory = self.fixture.state / name
            directory.chmod(0o755)
            self.assert_refused(["status", "--json"])
            directory.chmod(0o700)

    def test_unknown_fields_are_not_exposed_and_invalid_json_is_safe(self):
        files = self.prepare()
        for kind in files:
            path = files[kind][0]
            original = path.read_bytes()
            record = json.loads(original)
            record["extraPrivate"] = "synthetic-secret-marker"
            write_record(path, record)
            result = self.compare(["status", "--json"])
            self.assertNotIn("synthetic-secret-marker", json.dumps(result))
            path.write_text('{"password":"synthetic-secret-marker" INVALID}')
            self.assert_refused(["status", "--json"], partial=True)
            path.write_bytes(original)

    def test_frozen_does_not_inspect_tasks_and_human_status_skips_history(self):
        files = self.prepare()
        path = files["tasks"][0]
        original = path.read_bytes()
        path.write_text("invalid JSON")
        self.compare(["frozen", "--json"])
        path.write_bytes(original)
        retained = self.fixture.state / "removed"
        retained.mkdir(mode=0o755)
        self.compare(["status"])
        self.assert_refused(["status", "--json"])

    def test_real_source_created_records_are_accepted_without_runtime_dependencies(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        self.compare(["status", "--json"])
        expected = self.invoke(self.reference, ["status", "--json"])
        self.assertEqual(self.invoke([str(self.native)], ["status", "--json"], PATH="/no-executables-here"), expected)
        self.fixture.invoke(["freeze", "bench--sample", "--yes", "--json"])
        self.compare(["status", "--json"])
        self.compare(["frozen"])

    def test_status_and_frozen_tty_colors_match_and_json_stays_plain(self):
        self.prepare(2)
        for color, term in [(True, "xterm"), (False, "xterm"), (True, "dumb")]:
            for columns in [None, 40, 120]:
                for args in [["status"], ["status", "--json"], ["frozen"]]:
                    expected = self.invoke_tty(self.reference, args, color, term, columns)
                    actual = self.invoke_tty([str(self.native)], args, color, term, columns)
                    if "--json" in args:
                        self.assertEqual(json.loads(actual[1]), json.loads(expected[1]))
                        self.assertNotIn(b"\x1b", actual[1])
                        self.assertEqual((actual[0], actual[2]), (expected[0], expected[2]))
                    else:
                        self.assertEqual(actual, expected)

    def test_missing_legacy_optional_fields_and_integer_json_floats_match(self):
        files = self.prepare(1)
        receipt = json.loads(files["setups"][0].read_text())
        del receipt["runId"]
        del receipt["installOutputs"]
        del receipt["hooks"]
        del receipt["timings"]["finishedAt"]
        del receipt["timings"]["elapsedMs"]
        write_record(files["setups"][0], receipt)
        task = json.loads(files["tasks"][0].read_text())
        task["localPort"] = float(task["localPort"])
        task["httpsPort"] = float(task["httpsPort"])
        task["ports"]["api"] = float(task["ports"]["api"])
        write_record(files["tasks"][0], task)
        self.compare(["status", "--json"])

    def test_limits_and_unsafe_values_do_not_publish_ready_records(self):
        files = self.prepare(1)
        path = files["tasks"][0]
        original = json.loads(path.read_text())
        values = [{f"PUBLIC_{i}": "safe" for i in range(65)}, {"PORT": "x" * 4097},
                  {"PORT": "leading space "}, {"PORT": "line\nbreak"}, {"PORT": "value#comment"},
                  {"PORT": "{{ port.not-declared }}"}, {"PORT": "{{localPort}}}"}, {"PUBLIC_TOKEN": "synthetic-secret-marker"}]
        for value in values:
            record = copy.deepcopy(original)
            record["envOverrides"] = value
            write_record(path, record)
            self.assert_refused(["status", "--json"], partial=True)
        write_record(path, original)
