import copy
import json
from pathlib import Path
import shutil

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
import bench


class EventsTests(NativeCase):
    def fresh(self):
        if hasattr(self, "server"):
            self.server.close()
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def configure(self, auto=False, **changes):
        self.config = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(self.config.read_text())
        config.update(herdr="auto", **changes)
        config["projects"]["bench"]["autoSetup"] = auto
        self.config.write_text(json.dumps(config))
        self.server = LifecycleHerdr(self.fixture)
        self.addCleanup(self.server.close)
        self.env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": self.server.path, "HERDR_PANE_ID": "explicit-caller"}

    def event(self, command, value):
        result = self.invoke(command, ["_event"], **{**self.env, "HERDR_PLUGIN_EVENT_JSON": json.dumps(value)})
        return result[0], json.loads(result[1]) if result[1].startswith("{") else result[1], result[2]

    def snapshot(self):
        return {"event": "worktree_created", "data": {"type": "worktree_created", "workspace": {
            "workspace_id": "owned", "worktree": {"checkout_path": str(self.fixture.checkout), "repo_root": str(self.fixture.repo)}},
            "worktree": {"path": str(self.fixture.checkout), "branch": "task/sample"}}}

    def test_created_event_opt_in_setup_and_duplicate_guards(self):
        for auto in [False, True]:
            results, calls = [], []
            for command in [self.reference, [str(self.native)]]:
                self.fresh()
                self.configure(auto)
                self.fixture.git("worktree", "add", "-b", "task/sample", str(self.fixture.checkout), "HEAD")
                self.server.open = True
                self.server.tabs = [{"tab_id": "initial", "label": "1", "pane_count": 1}]
                event = self.snapshot()
                result = self.event(command, event)
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1]["status"], "ready" if auto else "unmanaged")
                duplicate = self.event(command, event)
                self.assertEqual(duplicate, (0, "", ""))
                self.assertEqual(len(list(self.fixture.state.glob("*.json"))), 1 if auto else 0)
                self.assertEqual((self.fixture.checkout / "node_modules").exists(), auto)
                results.append(bench.normalize([list(result), list(duplicate)], self.fixture.root))
                calls.append(bench.normalize(copy.deepcopy(self.server.calls), self.fixture.root))
                self.assertFalse(self.server.errors)
            self.assertEqual(results[0], results[1])
            self.assertEqual(calls[0], calls[1])

    def test_event_mismatched_snapshot_refuses_before_enrollment_or_native_mutation(self):
        self.configure(True)
        event = self.snapshot()
        event["data"]["worktree"]["path"] = str(self.fixture.repo)
        results = [self.event(command, event) for command in [self.reference, [str(self.native)]]]
        self.assertEqual(results[0], results[1])
        self.assertEqual(results[0][0], 1)
        self.assertIn("does not match", results[0][2])
        self.assertFalse(list(self.fixture.state.glob("*.json")))
        self.assertEqual(self.server.calls, [])

    def test_native_close_event_fences_setup_without_replaying_native_close(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure()
            self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
            self.server.open = False
            self.server.calls.clear()
            event = {"event": "workspace_closed", "data": {"type": "workspace_closed", "workspace_id": "owned",
                "workspace": {"worktree": {"checkout_path": str(self.fixture.checkout)}}}}
            result = self.event(command, event)
            self.assertEqual(result[0], 0, result)
            self.assertEqual(result[1]["cleanupStatus"], "complete")
            task = json.loads((self.fixture.state / "bench--sample.json").read_text())
            receipt = json.loads(next((self.fixture.state / "setup").glob("*.json")).read_text())
            self.assertEqual(task["status"], "closed")
            self.assertEqual(receipt["status"], "ready")
            self.assertTrue(self.fixture.checkout.exists())
            self.assertEqual(self.server.calls, [])
            results.append(bench.normalize([list(result), task, receipt], self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_native_removal_retires_records_only_after_git_confirms_checkout_is_gone(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure()
            self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
            self.server.open = False
            event = {"event": "worktree_removed", "data": {"type": "worktree_removed", "workspace_id": "owned",
                "worktree": {"path": str(self.fixture.checkout)}}}
            refused = self.event(command, event)
            self.assertEqual(refused[0], 1)
            self.assertTrue((self.fixture.state / "bench--sample.json").exists())
            self.fixture.git("worktree", "remove", str(self.fixture.checkout))
            self.server.calls.clear()
            result = self.event(command, event)
            self.assertEqual(result[0], 0, result)
            self.assertEqual(result[1]["cleanupStatus"], "complete")
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            self.assertFalse(list((self.fixture.state / "setup").glob("*.json")))
            self.assertEqual(self.server.calls, [])
            results.append(bench.normalize([list(refused), list(result)], self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_reconcile_closes_only_missing_workspaces_and_startup_thaw_can_be_disabled(self):
        results, calls = [], []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure(startupThaw=False)
            self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
            self.server.open = False
            self.server.calls.clear()
            startup = self.invoke(command, ["_startup"], **self.env)
            self.assertEqual(startup[0], 0, startup)
            report = json.loads(startup[1])
            self.assertEqual(report["thaw"]["status"], "disabled")
            self.assertEqual(json.loads((self.fixture.state / "bench--sample.json").read_text())["status"], "closed")
            self.assertTrue(self.fixture.checkout.exists())
            reconcile = self.invoke(command, ["reconcile", "--json"])
            self.assertEqual(reconcile[0], 0, reconcile)
            self.assertTrue(all(c["method"] == "workspace.list" for c in self.server.calls))
            results.append(bench.normalize([[startup[0], report, startup[2]], list(reconcile)], self.fixture.root))
            calls.append(copy.deepcopy(self.server.calls))
        self.assertEqual(results[0], results[1])
        self.assertEqual(calls[0], calls[1])

    def test_startup_still_checks_returns_when_reconciliation_needs_attention(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure(startupThaw=True)
            self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
            self.server.close()
            result = self.invoke(command, ["_startup"], **self.env)
            self.assertEqual(result[0], 1, result)
            report = json.loads(result[1])
            self.assertEqual(report["reconciliation"]["status"], "attention")
            self.assertEqual(report["thaw"]["status"], "completed")
            # Platform socket error wording differs; the failure status and continuation do not.
            self.assertTrue(report["reconciliation"]["message"])
            report["reconciliation"]["message"] = "<socket unavailable>"
            results.append(report)
        self.assertEqual(results[0], results[1])
