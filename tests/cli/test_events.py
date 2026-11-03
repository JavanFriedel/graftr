import copy
import fcntl
import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
from status_fixture import write_record
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

    def test_reconcile_releases_registry_lock_while_waiting_for_herdr(self):
        self.configure()
        self.assertEqual(self.invoke([str(self.native)], ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
        self.server.open = False
        response = self.server.response
        available = []
        def probe(method, params):
            if method == "workspace.list":
                with (self.fixture.state / "operation.lock").open("r+") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        available.append(True)
                    except BlockingIOError:
                        available.append(False)
            return response(method, params)
        self.server.response = probe
        result = self.invoke([str(self.native)], ["reconcile", "--json"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual(available, [True])
        self.assertEqual(json.loads((self.fixture.state / "bench--sample.json").read_text())["status"], "closed")
        self.assertFalse(self.server.errors)

    def test_plain_create_intent_suppresses_reentrant_and_late_auto_setup_events(self):
        self.configure(auto=True)
        response = self.server.response
        events = []
        def creating(method, params):
            result = response(method, params)
            if method == "worktree.create":
                self.assertEqual(len(list((self.fixture.state / "creating").glob("*.json"))), 1)
                event = self.snapshot()
                child = subprocess.run([str(self.native), "_event"], cwd=self.fixture.repo,
                                       env={**self.fixture.env, **self.env, "HERDR_PLUGIN_EVENT_JSON": json.dumps(event)},
                                       stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=3)
                events.append((child.returncode, child.stdout, child.stderr))
            return result
        self.server.response = creating
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-setup", "--json"], **self.env)
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["data"]["status"], "skipped")
        self.assertEqual(events, [(0, "", "")])
        self.assertEqual(self.event([str(self.native)], self.snapshot()), (0, "", ""))
        self.assertFalse((self.fixture.state / "bench--sample.json").exists())
        self.assertFalse((self.fixture.checkout / ".env").exists())
        self.assertFalse((self.fixture.checkout / "node_modules").exists())
        self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
        receipts = list((self.fixture.state / "setup").glob("*.json"))
        self.assertEqual(len(receipts), 1)
        self.assertEqual(json.loads(receipts[0].read_text())["status"], "skipped")
        self.assertFalse(self.server.errors)

    def test_reconcile_refuses_changed_snapshot_before_any_closure(self):
        for kind in ("task", "setup", "added", "removed"):
            with self.subTest(kind=kind):
                self.fresh()
                self.configure()
                self.assertEqual(self.invoke([str(self.native)], ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
                path = self.fixture.state / "bench--sample.json"
                task = json.loads(path.read_text())
                receipt_path = next((self.fixture.state / "setup").glob("*.json"))
                receipt = json.loads(receipt_path.read_text())
                newer = copy.deepcopy(receipt if kind == "setup" else task)
                if kind == "task":
                    newer["status"] = "cleaned"
                elif kind == "setup":
                    newer["message"] = "Newer setup observation"
                elif kind == "added":
                    newer["id"] = "bench--added"
                self.server.open = False
                response = self.server.response
                def change(method, params):
                    if method == "workspace.list":
                        with (self.fixture.state / "operation.lock").open("r+") as lock:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            if kind == "removed":
                                path.unlink()
                            else:
                                destination = receipt_path if kind == "setup" else self.fixture.state / "bench--added.json" if kind == "added" else path
                                write_record(destination, newer)
                    return response(method, params)
                self.server.response = change
                result = self.invoke([str(self.native)], ["reconcile", "--json"])
                self.assertEqual(result[0], 1, result)
                self.assertIn("changed during reconciliation", result[1]["error"]["message"])
                if kind == "removed":
                    self.assertFalse(path.exists())
                else:
                    self.assertEqual(json.loads(path.read_text()), newer if kind == "task" else task)
                self.assertEqual(json.loads(receipt_path.read_text()), newer if kind == "setup" else receipt)
                if kind == "added":
                    self.assertEqual(json.loads((self.fixture.state / "bench--added.json").read_text()), newer)
                self.assertFalse(self.server.errors)
