import json
import os

from test_cli import NativeCase
from status_fixture import seed_status, write_record
import bench


class RecordIsolationTests(NativeCase):
    def partial(self, args):
        before = bench.readonly_snapshot(self.fixture)
        result = self.invoke([str(self.native)], [*args, "--json"])
        self.assertEqual((result[0], result[2]), (1, ""), result)
        report = result[1]
        self.assertEqual((report["ok"], report["outcome"], report["error"]["code"]),
                         (False, "partial", "RECORDS_INCOMPLETE"))
        self.assertTrue(report["data"]["recordIssues"])
        self.assertNotIn("synthetic-secret-marker", json.dumps(report))
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        return report["data"]

    def test_status_preserves_valid_records_when_any_single_category_file_is_invalid(self):
        files = seed_status(self.fixture, 2)
        baseline = self.invoke([str(self.native)], ["status", "--json"])[1]["data"]
        for kind in ("tasks", "setups", "adopted", "frozen"):
            path = files[kind][0]
            original = path.read_bytes()
            path.write_text('{"password":"synthetic-secret-marker" INVALID}')
            data = self.partial(["status"])
            self.assertEqual(len(data[kind]), len(baseline[kind]) - 1, kind)
            for other in ("tasks", "setups", "adopted", "frozen"):
                if other != kind:
                    self.assertEqual(data[other], baseline[other], (kind, other))
            self.assertEqual([issue["path"] for issue in data["recordIssues"]],
                             [path.relative_to(self.fixture.state).as_posix()])
            self.assertEqual(data["recordIssues"][0]["code"], "INVALID_RECORD")
            if kind == "frozen":
                frozen = self.invoke([str(self.native)], ["frozen", "--json"])
                self.assertEqual((frozen[0], frozen[1]["outcome"], frozen[1]["data"]), (1, "failed", None))
            path.write_bytes(original)
        self.assertEqual(self.invoke([str(self.native)], ["status", "--json"])[1]["data"], baseline)

    def test_unsafe_files_filename_mismatches_and_special_files_are_never_loaded(self):
        files = seed_status(self.fixture, 2)
        path = files["tasks"][0]
        original = path.read_bytes()
        path.chmod(0o644)
        self.partial(["status"])
        path.chmod(0o600)
        target = path.with_name("private-original")
        path.rename(target)
        path.symlink_to(target.name)
        self.partial(["status"])
        path.unlink()
        os.mkfifo(path, 0o600)
        self.partial(["status"])
        path.unlink()
        path.mkdir(mode=0o700)
        self.partial(["status"])
        path.rmdir()
        target.rename(path)
        wrong = path.with_name("bench--wrong.json")
        path.rename(wrong)
        self.partial(["status"])
        wrong.rename(path)
        self.assertEqual(path.read_bytes(), original)

    def test_invalid_record_names_are_escaped_in_human_output_and_sorted_in_json(self):
        seed_status(self.fixture, 1)
        names = ["z-bad.json", "a-\x1b[31m\nprivate.json"]
        for name in names:
            write_record(self.fixture.state / name, {"private": "synthetic-secret-marker"})
        data = self.partial(["status"])
        self.assertEqual([issue["path"] for issue in data["recordIssues"]], sorted(names))
        for width in [24, 80]:
            result = self.invoke([str(self.native)], ["status"], COLUMNS=str(width))
            self.assertEqual((result[0], result[2]), (1, ""))
            self.assertIn("Incomplete report", result[1])
            self.assertNotIn("\x1b", result[1])
            self.assertNotIn("synthetic-secret-marker", result[1])
            warning = result[1].split("\n\n", 1)[0]
            self.assertTrue(all(len(line) <= width for line in warning.splitlines()))

    def test_unsafe_category_directories_still_fail_closed(self):
        seed_status(self.fixture, 1)
        for name in ("setup", "adopted", "frozen"):
            root = self.fixture.state / name
            root.chmod(0o755)
            result = self.invoke([str(self.native)], ["status", "--json"])
            self.assertEqual((result[0], result[1]["outcome"], result[1]["data"]), (1, "failed", None))
            root.chmod(0o700)

    def test_all_invalid_records_do_not_claim_the_registry_is_empty(self):
        files = seed_status(self.fixture, 1)
        for paths in files.values():
            for path in paths:
                path.write_text("invalid JSON synthetic-secret-marker")
        data = self.partial(["status"])
        for category in ("tasks", "setups", "adopted", "frozen"):
            self.assertEqual(data[category], [])
        self.assertEqual(len(data["recordIssues"]), sum(len(paths) for paths in files.values()))
        human = self.invoke([str(self.native)], ["status"], COLUMNS="120")
        self.assertEqual(human[0], 1)
        self.assertIn("No validated worktrees or frozen tasks to display.", human[1])
        self.assertNotIn("No managed worktrees or frozen tasks.", human[1])

    def test_inventory_keeps_git_rows_and_does_not_claim_incomplete_tracking_is_unmanaged(self):
        files = seed_status(self.fixture, 2)
        checkout = self.fixture.root / "manual"
        self.fixture.git("worktree", "add", "-qb", "feature/manual", str(checkout))
        baseline = self.invoke([str(self.native)], ["list", "bench", "--json"])[1]["data"]
        self.assertEqual(next(tree for tree in baseline["worktrees"] if tree["path"] == str(checkout))["tracking"], "unmanaged")
        path = files["tasks"][0]
        path.write_text('{"private":"synthetic-secret-marker" INVALID}')
        data = self.partial(["list", "bench"])
        tree = next(tree for tree in data["worktrees"] if tree["path"] == str(checkout))
        self.assertEqual(tree["tracking"], "unknown")
        self.assertIn("incomplete-tracking", tree["issues"])
        self.assertTrue(any(task["id"] == "bench--item-001" for tree in data["worktrees"] for task in tree["tasks"]))
        human = self.invoke([str(self.native)], ["list", "bench"])
        self.assertEqual(human[0], 1)
        self.assertIn("Incomplete report", human[1])

    def test_invalid_setup_does_not_hide_known_inventory_tracking(self):
        files = seed_status(self.fixture, 2)
        path = files["setups"][0]
        path.write_text("invalid JSON")
        data = self.partial(["list", "bench"])
        tree = next(tree for tree in data["worktrees"] if any(task["id"] == "bench--item-001" for task in tree["tasks"]))
        self.assertEqual(tree["tracking"], "managed")
        self.assertNotIn("incomplete-tracking", tree["issues"])

    def test_invalid_record_cannot_be_ignored_during_port_allocation(self):
        seed_status(self.fixture, 1)
        invalid = self.fixture.state / "bench--item-000.json"
        invalid.write_text('{"private":"synthetic-secret-marker" INVALID}')
        before = invalid.read_bytes()
        result = self.invoke([str(self.native)], ["create", "bench", "must-not-exist", "--no-herdr", "--json"])
        self.assertEqual(result[0], 1)
        self.assertFalse((self.fixture.root / "trees/must-not-exist").exists())
        self.assertNotIn("task/must-not-exist", self.fixture.git("branch", "--list"))
        self.assertEqual(invalid.read_bytes(), before)
        self.assertNotIn("synthetic-secret-marker", json.dumps(result))
