import copy
import json
from pathlib import Path
import sys

from test_cli import NativeCase

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "benchmarks"))
from status_fixture import seed_status, write_record, plan
import bench


class StatusTests(NativeCase):
    def prepare(self, count=4):
        return seed_status(self.fixture, count)

    def status(self, *args, cwd=None, **env):
        before = bench.readonly_snapshot(self.fixture)
        result = self.invoke([str(self.native)], ["status", *args], cwd=cwd, **env)
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        self.assertFalse((self.fixture.root / "home/forbidden-service").exists())
        return result

    def test_created_worktrees_have_focused_status_and_keep_existing_targeted_commands(self):
        for name in ["sample", "second"]:
            result = self.invoke([str(self.native)], ["create", "bench", name, "--no-herdr", "--json"])
            self.assertEqual(result[0], 0, result)
        checkout = self.fixture.checkout
        result = self.status("--json", cwd=checkout)
        self.assertEqual([task["id"] for task in result[1]["data"]["tasks"]], ["bench--sample"])
        self.assertEqual(result[1]["data"]["setups"][0]["status"], "ready")
        self.assertEqual(len(self.status("--all", "--json", cwd=checkout)[1]["data"]["tasks"]), 2)
        for args in [["logs", "sample"], ["doctor", "task/sample"], ["close", "./trees/sample"],
                     ["open", "bench--sample", "--no-herdr"]]:
            result = self.invoke([str(self.native)], [*args, "--json"], cwd=self.fixture.root)
            self.assertEqual(result[0], 0, result)
        self.assertFalse((self.fixture.root / "home/forbidden-service").exists())

    def test_default_scope_is_current_worktree_including_nested_and_symlinked_directories(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        checkout = Path(task["path"])
        nested = checkout / "nested/subdir"
        nested.mkdir(parents=True)
        link = self.fixture.root / "link"
        link.symlink_to(checkout)
        for cwd in [checkout, nested, link / "nested"]:
            result = self.status("--json", cwd=cwd, PATH="/no-executables-here")
            self.assertEqual((result[0], result[2]), (0, ""), result)
            data = result[1]["data"]
            self.assertEqual([item["id"] for item in data["tasks"]], [task["id"]])
            self.assertEqual([item["taskId"] for item in data["setups"]], [task["id"]])
            self.assertEqual((data["adopted"], data["frozen"], data["retained"]), ([], [], []))
        result = self.status(cwd=nested, COLUMNS="120")
        self.assertEqual(result[0], 0, result)
        for value in [task["id"], task["branch"], task["path"], "Installer failed", "--all"]:
            self.assertIn(value, result[1])
        self.assertNotIn("item-000", result[1])
        self.assertNotIn("Frozen / recovery", result[1])
        self.assertNotIn("synthetic-private", result[1])

    def test_outside_managed_worktrees_and_all_keep_the_global_overview(self):
        files = self.prepare()
        task = json.loads(files["tasks"][0].read_text())
        checkout = Path(task["path"])
        checkout.mkdir(parents=True)
        unmanaged = self.fixture.root / "unmanaged"
        self.fixture.git("worktree", "add", "-qb", "feature/unmanaged", str(unmanaged))
        prefix_sibling = checkout.with_name(checkout.name + "-unmanaged")
        prefix_sibling.mkdir()
        global_result = self.status("--all", "--json", PATH="/no-executables-here")
        self.assertEqual(global_result[0], 0, global_result)
        self.assertEqual(len(global_result[1]["data"]["tasks"]), 4)
        self.assertEqual(len(global_result[1]["data"]["frozen"]), 4)
        for cwd in [self.fixture.repo, self.fixture.root, unmanaged, prefix_sibling]:
            self.assertEqual(self.status("--json", cwd=cwd), global_result)
        self.assertEqual(self.status("--all", "--json", cwd=checkout), global_result)

    def test_explicit_targets_use_existing_exact_selectors_and_project_scope(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        checkout = Path(task["path"])
        (checkout / "nested").mkdir(parents=True)
        expected = self.status("--json", cwd=checkout)
        for selector in [task["id"], task["branch"], checkout.name, str(checkout),
                         "./trees/item-001/nested", "trees/item-001/"]:
            self.assertEqual(self.status(selector, "--json", cwd=self.fixture.root), expected)
        self.assertEqual(self.status("@", "--json", cwd=checkout), expected)
        self.assertEqual(self.status(checkout.name, "--project", "bench", "--json"), expected)
        checkout.joinpath("file").write_text("fixture")
        for args in [["missing"], ["@"], ["--project", "bench"], [task["id"], "--project", "other"],
                     [str(checkout / "file")], [str(checkout / "missing")]]:
            result = self.status(*args, "--json", cwd=self.fixture.root)
            self.assertEqual(result[0], 1, (args, result))
            self.assertIsNone(result[1]["data"])
        self.assertEqual(self.status(task["id"], "--json", PATH="/no-executables-here"), expected)
        # A recorded missing checkout is still inspectable, without live Git checks.
        missing = json.loads(files["tasks"][2].read_text())
        self.assertEqual(self.status(missing["path"], "--json")[1]["data"]["tasks"][0]["id"], missing["id"])

    def test_adopted_and_frozen_targets_can_be_inspected_without_provisioning(self):
        files = self.prepare()
        adopted = json.loads(files["adopted"][0].read_text())
        nested = Path(adopted["path"]) / "nested"
        nested.mkdir(parents=True)
        expected = self.status("--json", cwd=nested)
        self.assertEqual(expected[0], 0, expected)
        self.assertEqual([r["id"] for r in expected[1]["data"]["adopted"]], [adopted["id"]])
        self.assertEqual(expected[1]["data"]["tasks"], [])
        for selector in [adopted["id"], adopted["branch"], adopted["path"], "./external/0", "0"]:
            self.assertEqual(self.status(selector, "--json", cwd=self.fixture.root), expected)
        human = self.status(cwd=nested)[1]
        self.assertIn("adopted", human)
        self.assertIn("external", human)
        self.assertNotIn("localhost", human)
        frozen = json.loads(files["frozen"][0].read_text())
        for selector in [frozen["id"], frozen["removal"]["path"], frozen["removal"]["currentBranch"], "stored-0"]:
            result = self.status(selector, "--json", cwd=self.fixture.root)
            self.assertEqual(result[0], 0, result)
            self.assertEqual([r["id"] for r in result[1]["data"]["frozen"]], [frozen["id"]])
            self.assertEqual(result[1]["data"]["tasks"], [])

    def test_ambiguous_targets_are_errors_and_full_ids_remain_authoritative(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        other = copy.deepcopy(task)
        other.update(id="other--duplicate", project="other", repo=str(self.fixture.root / "other-repo"),
                     path=str(self.fixture.root / "other/item-001"))
        other["overrides"]["PUBLIC_SLUG"] = "other_item_001"
        for path in files["adopted"]:
            adopted = json.loads(path.read_text())
            adopted["repo"] = other["repo"]
            write_record(path, adopted)
        write_record(self.fixture.state / (other["id"] + ".json"), other)
        result = self.status("item-001", "--json", cwd=self.fixture.root)
        self.assertEqual(result[1]["error"]["code"], "TARGET_AMBIGUOUS")
        self.assertEqual(len(result[1]["error"]["details"]["candidates"]), 2)
        self.assertEqual(self.status("item-001", "--project", "other", "--json")[1]["data"]["tasks"][0]["id"], other["id"])
        self.assertEqual(self.status("item-001", "--json")[1]["data"]["tasks"][0]["id"], task["id"])
        # Full IDs and explicit paths can cross the inferred project boundary.
        self.assertEqual(self.status(other["id"], "--json")[1]["data"]["tasks"][0]["id"], other["id"])
        self.assertEqual(self.status(other["path"], "--json")[1]["data"]["tasks"][0]["id"], other["id"])
        other["path"] = task["path"]
        write_record(self.fixture.state / (other["id"] + ".json"), other)
        Path(task["path"]).mkdir(parents=True)
        result = self.status("--json", cwd=Path(task["path"]))
        self.assertEqual(result[1]["error"]["code"], "TARGET_AMBIGUOUS")

    def test_focused_status_preserves_all_receipts_and_report_wide_validation_warnings(self):
        files = self.prepare()
        task = json.loads(files["tasks"][0].read_text())
        Path(task["path"]).mkdir(parents=True)
        files["tasks"][1].write_text('{"private":"synthetic-secret-marker" INVALID}')
        result = self.status("--json", cwd=Path(task["path"]))
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertEqual(result[1]["error"]["code"], "RECORDS_INCOMPLETE")
        self.assertEqual(len(result[1]["data"]["tasks"]), 1)
        self.assertEqual(len(result[1]["data"]["setups"]), 2)
        self.assertEqual(len(result[1]["data"]["recordIssues"]), 1)
        self.assertNotIn("synthetic-secret-marker", str(result))
        result = self.status(cwd=Path(task["path"]))
        self.assertEqual(result[0], 1, result)
        self.assertIn("Multiple setup receipts need review", result[1])

    def test_focused_json_does_not_read_global_retained_history(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        (self.fixture.state / "removed").mkdir(mode=0o755)
        self.assertEqual(self.status(task["id"], "--json")[0], 0)
        self.assertEqual(self.status("--all", "--json")[0], 1)

    def test_focused_status_keeps_matching_recovery_without_an_ambiguous_target(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        frozen = json.loads(files["frozen"][1].read_text())
        frozen["id"] = frozen["removal"]["id"] = task["id"]
        frozen["removal"].update(path=task["path"], branch=task["branch"], currentBranch=task["branch"])
        write_record(self.fixture.state / "frozen" / (task["id"] + ".json"), frozen)
        Path(task["path"]).mkdir(parents=True)
        result = self.status("--json", cwd=Path(task["path"]))
        self.assertEqual(result[0], 0, result)
        self.assertEqual([r["id"] for r in result[1]["data"]["tasks"]], [task["id"]])
        self.assertEqual([r["id"] for r in result[1]["data"]["frozen"]], [task["id"]])
        for columns in [1, 16, 24, 40, 60, 80, 120]:
            result = self.status(task["id"], COLUMNS=str(columns))
            self.assertEqual(result[0], 0, result)
            self.assertTrue(all(len(line) <= columns for line in result[1].splitlines()))

    def test_focused_heading_escapes_control_characters_in_recorded_paths(self):
        files = self.prepare()
        path = files["adopted"][0]
        adopted = json.loads(path.read_text())
        adopted["path"] += "\nunsafe\x1b[31m"
        write_record(path, adopted)
        result = self.status(adopted["id"], COLUMNS="1000")
        self.assertEqual(result[0], 0, result)
        self.assertNotIn("\x1b", result[1])
        self.assertNotIn("\nunsafe", result[1])
        self.assertIn("\\u000aunsafe", result[1])

    def test_status_rejects_conflicting_flags_and_extra_targets(self):
        for args in [["--all", "bench--sample"], ["--all", "@"], ["--all", "--project", "bench"],
                     ["one", "two"], ["--all=true"]]:
            result = self.status(*args, "--json")
            self.assertEqual(result[0], 1, result)
            self.assertEqual(result[1]["error"]["code"], "INVALID_ARGUMENTS")
        result = self.invoke([str(self.native)], ["list", "bench", "--all", "--json"])
        self.assertEqual(result[1]["error"]["code"], "INVALID_ARGUMENTS")
        self.assertFalse(self.fixture.state.exists())

    def test_status_completion_offers_flags_and_managed_adopted_targets(self):
        files = self.prepare()
        task = json.loads(files["tasks"][1].read_text())
        result = self.compare(["_complete", "--", "status", "--"])
        self.assertIn("--all", result[1].splitlines())
        self.assertIn("--project", result[1].splitlines())
        for prefix, candidate in [("item-001", "item-001"), ("bench--item-001", task["id"]),
                                  ("other--external", "other--external-0")]:
            result = self.compare(["_complete", "--", "status", prefix])
            self.assertIn(candidate, result[1].splitlines())
        self.assertEqual(self.compare(["_complete", "--", "status", "--all", ""]), (0, "", ""))
        result = self.compare(["_complete", "--", "status", "--all", "--"])
        self.assertNotIn("--project", result[1].splitlines())
        result = self.compare(["_complete", "--", "status", task["id"], "--"])
        self.assertNotIn("--all", result[1].splitlines())

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

    def test_combined_status_has_one_column_and_does_not_change_json(self):
        files = self.prepare()
        task_path = files["tasks"][1]
        task = json.loads(task_path.read_text())
        receipt_path = next(path for path in files["setups"] if json.loads(path.read_text())["taskId"] == task["id"])
        receipt = json.loads(receipt_path.read_text())
        task["status"] = "ready"
        receipt.update(status="ready", step="complete", dependencies="ready")
        write_record(task_path, task)
        write_record(receipt_path, receipt)
        original_json = self.status(task["id"], "--json")
        human = self.status(task["id"], COLUMNS="200")
        self.assertEqual(human[0], 0, human)
        self.assertIn("STATUS", human[1])
        self.assertNotIn("STATE", human[1])
        self.assertNotIn("SETUP", human[1])
        self.assertIn("ready", human[1])
        self.assertNotIn("prepared", human[1])
        self.assertEqual(self.status(task["id"], "--json"), original_json)
        self.assertEqual(original_json[1]["data"]["tasks"][0]["status"], "ready")
        self.assertEqual(original_json[1]["data"]["setups"][0]["status"], "ready")
        for lifecycle, setup, expected in [
            ("preparing", "pending", "setting up (dependencies)"),
            ("preparing", "incomplete", "setup incomplete (dependencies)"),
            ("closed", "ready", "closed"),
            ("closed", "incomplete", "closed — setup incomplete (dependencies)"),
            ("ready", "skipped", "setup skipped"),
            ("ready", "unmanaged", "setup unmanaged"),
        ]:
            task["status"] = lifecycle
            receipt.update(status=setup, step="dependencies")
            write_record(task_path, task)
            write_record(receipt_path, receipt)
            human = self.status(task["id"], COLUMNS="200")
            self.assertEqual(human[0], 0, human)
            self.assertIn(expected, human[1])
        task["status"] = "closed"
        write_record(task_path, task)
        receipt_path.unlink()
        self.assertIn("closed — setup unknown", self.status(task["id"], COLUMNS="200")[1])

    def test_combined_status_does_not_hide_cleanup_or_retirement_failures(self):
        files = self.prepare()
        task_path = files["tasks"][3]
        task = json.loads(task_path.read_text())
        task["cleanup"] = plan("cleanup", "failed")
        write_record(task_path, task)
        human = self.status(task["id"], COLUMNS="240")
        self.assertEqual(human[0], 0, human)
        self.assertIn("closed — cleanup failed — retirement failed", human[1])
        self.assertNotIn("synthetic-private", human[1])
        human = self.status("--all", COLUMNS="240")
        self.assertIn("adopted — external setup", human[1])
        self.assertIn("adopted — needs review", human[1])
        self.assertIn("setup needs review", human[1])
        self.assertNotIn("STATE", human[1])
        self.assertNotIn("SETUP", human[1])
        self.assertIn("STATUS", self.compare(["frozen"], COLUMNS="240")[1])

    def test_combined_status_keeps_long_statuses_and_addresses_at_narrow_widths(self):
        files = self.prepare()
        task = json.loads(files["tasks"][0].read_text())
        for columns in [1, 16, 24, 40, 60, 80, 120]:
            human = self.status(task["id"], COLUMNS=str(columns))
            self.assertEqual(human[0], 0, human)
            compact = "".join(human[1].split())
            for value in ["setup needs review", "cleanup failed", "retirement failed", task["url"]]:
                self.assertIn("".join(value.split()), compact)
            self.assertTrue(all(len(line) <= columns for line in human[1].splitlines()))

    def test_status_column_colors_ready_and_problem_labels_and_respects_no_color(self):
        files = self.prepare()
        task_path = files["tasks"][1]
        task = json.loads(task_path.read_text())
        task["status"] = "ready"
        write_record(task_path, task)
        receipt_path = next(path for path in files["setups"] if json.loads(path.read_text())["taskId"] == task["id"])
        receipt = json.loads(receipt_path.read_text())
        receipt.update(status="ready", step="complete", dependencies="ready")
        write_record(receipt_path, receipt)
        before = bench.readonly_snapshot(self.fixture)
        for target, label, code in [(task["id"], "ready", 32), ("bench--item-003", "closed — retirement failed", 31)]:
            for color, term in [(True, "xterm"), (False, "xterm"), (True, "dumb")]:
                result = self.invoke_tty([str(self.native)], ["status", target], color, term, 240)
                self.assertEqual(result[0], 0, result)
                if color and term != "dumb":
                    self.assertIn(f"\x1b[{code}m{label}\x1b[0m".encode(), result[1])
                else:
                    self.assertNotIn(b"\x1b", result[1])
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)

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
