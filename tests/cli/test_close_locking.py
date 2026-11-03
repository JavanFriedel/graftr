import fcntl
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
from status_fixture import plan, receipt_path, write_record
import bench


class CloseLockingTests(NativeCase):
    def prepare(self):
        if hasattr(self, "server"):
            self.server.close()
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"])
        self.assertEqual(result[0], 0, result)
        # Run ordinary daily maintenance before taking no-mutation snapshots.
        result = self.invoke([str(self.native)], ["reconcile", "--json"])
        self.assertEqual(result[0], 0, result)
        self.server = LifecycleHerdr(self.fixture)
        self.server.open = True
        self.addCleanup(self.server.close)
        self.record = self.fixture.state / "bench--sample.json"
        self.task = json.loads(self.record.read_text())
        self.receipt = next((self.fixture.state / "setup").glob("*.json"))
        self.attach(self.server.path)

    def attach(self, socket):
        self.task.update(socket=socket, workspaceId="owned")
        write_record(self.record, self.task)
        receipt = json.loads(self.receipt.read_text())
        self.receipt.unlink()
        receipt.update(socket=socket, workspaceId="owned")
        self.receipt = write_record(receipt_path(self.fixture.state, receipt), receipt)

    def pending(self):
        self.task["status"] = "preparing"
        write_record(self.record, self.task)
        receipt = json.loads(self.receipt.read_text())
        receipt.update(status="pending", step="dependencies", dependencies="pending")
        write_record(self.receipt, receipt)

    def close(self):
        return self.invoke([str(self.native)], ["close", self.task["id"], "--json"])

    def test_close_inventory_releases_registry_lock_but_close_mutation_stays_serialized(self):
        self.prepare()
        observations = []
        response = self.server.response
        def probe(method, params):
            with (self.fixture.state / "operation.lock").open("r+") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    available = True
                except BlockingIOError:
                    available = False
            observations.append((method, available))
            return response(method, params)
        self.server.response = probe
        result = self.close()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(observations, [("workspace.list", True), ("workspace.close", False)])
        self.assertEqual(json.loads(self.record.read_text())["status"], "closed")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(self.server.open)
        self.assertFalse(self.server.errors)

    def test_unrelated_creation_can_finish_while_close_inventory_is_pending(self):
        self.prepare()
        response = self.server.response
        creations = []
        def create(method, params):
            if method == "workspace.list":
                child = subprocess.run([str(self.native), "create", "bench", "other", "--no-herdr", "--json"],
                                       cwd=self.fixture.repo, env=self.fixture.env, stdin=subprocess.DEVNULL,
                                       capture_output=True, text=True, timeout=3)
                creations.append((child.returncode, json.loads(child.stdout), child.stderr))
            return response(method, params)
        self.server.response = create
        result = self.close()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(len(creations), 1)
        self.assertEqual((creations[0][0], creations[0][2]), (0, ""), creations)
        other = json.loads((self.fixture.state / "bench--other.json").read_text())
        self.assertEqual(other["status"], "ready")
        self.assertEqual(json.loads(self.record.read_text())["status"], "closed")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(self.server.errors)

    def test_changed_context_refuses_before_cancellation_cleanup_or_close(self):
        for kind in ("task", "receipt", "attachment", "path", "conflicting-path", "conflicting-workspace", "removed-task", "removed-receipt"):
            with self.subTest(kind=kind):
                self.prepare()
                self.pending()
                response = self.server.response
                saved = []
                def change(method, params):
                    if method == "workspace.list":
                        with (self.fixture.state / "operation.lock").open("r+") as lock:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            if kind == "task":
                                write_record(self.record, {**self.task, "status": "closed"})
                            elif kind == "receipt":
                                receipt = json.loads(self.receipt.read_text())
                                write_record(self.receipt, {**receipt, "runId": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"})
                            elif kind == "attachment":
                                write_record(self.record, {**self.task, "workspaceId": "new-owner"})
                            elif kind == "path":
                                write_record(self.record, {**self.task, "path": str(self.fixture.root / "new-checkout")})
                            elif kind.startswith("conflicting"):
                                other = {**self.task, "id": "bench--conflicting"}
                                if kind == "conflicting-path":
                                    other["workspaceId"] = "other-workspace"
                                else:
                                    other["path"] = str(self.fixture.root / "other-checkout")
                                write_record(self.fixture.state / "bench--conflicting.json", other)
                            elif kind == "removed-task":
                                self.record.unlink()
                            else:
                                self.receipt.unlink()
                        saved.append(bench.readonly_snapshot(self.fixture))
                    return response(method, params)
                self.server.response = change
                result = self.close()
                self.assertEqual(result[0], 1, result)
                self.assertEqual(result[1]["outcome"], "failed")
                self.assertEqual(result[1]["progress"], [])
                self.assertEqual(len(saved), 1)
                self.assertEqual(bench.readonly_snapshot(self.fixture), saved[0])
                self.assertEqual([call["method"] for call in self.server.calls], ["workspace.list"])
                self.assertTrue(self.server.open)
                self.assertFalse(self.server.errors)

    def test_absent_workspace_closes_record_without_replaying_a_native_close(self):
        self.prepare()
        self.server.open = False
        result = self.close()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(json.loads(self.record.read_text())["status"], "closed")
        self.assertEqual([call["method"] for call in self.server.calls], ["workspace.list"])
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(self.server.errors)

    def test_wrong_checkout_or_malformed_inventory_refuses_without_state_changes(self):
        for kind in ("wrong-checkout", "malformed"):
            with self.subTest(kind=kind):
                self.prepare()
                self.pending()
                response = self.server.response
                def drift(method, params):
                    result = response(method, params)
                    if kind == "wrong-checkout":
                        result["workspaces"][-1]["worktree"]["checkout_path"] = str(self.fixture.repo)
                    else:
                        result["workspaces"] = "synthetic-secret-response"
                    return result
                self.server.response = drift
                before = bench.readonly_snapshot(self.fixture)
                result = self.close()
                self.assertEqual(result[0], 1, result)
                self.assertEqual(result[1]["progress"], [])
                self.assertNotIn("synthetic-secret-response", json.dumps(result))
                self.assertEqual(bench.readonly_snapshot(self.fixture), before)
                self.assertTrue(self.server.open)
                self.assertFalse(self.server.errors)

    def test_pending_setup_can_be_fenced_without_waiting_for_the_installers_worktree_lock(self):
        self.prepare()
        self.pending()
        key = hashlib.sha256(self.task["path"].encode()).hexdigest()
        with (self.fixture.state / f"setup-{key}.lock").open("r+") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.close()
        self.assertEqual(result[0], 0, result)
        receipt = json.loads(self.receipt.read_text())
        self.assertEqual(receipt["status"], "incomplete")
        self.assertIsNone(receipt["runId"])
        self.assertEqual(json.loads(self.record.read_text())["status"], "closed")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(self.server.errors)

    def test_lost_close_reply_preserves_fencing_queued_cleanup_and_uncertain_progress(self):
        self.prepare()
        self.server.close()
        effects = []
        def respond(request):
            if request["method"] == "workspace.list":
                return {"id": request["id"], "result": {"workspaces": [self.server.workspace()]}}
            self.assertEqual(request["method"], "workspace.close")
            effects.append(request["params"])
            return None
        socket, calls, errors = self.serve(respond, count=2)
        self.attach(socket)
        self.pending()
        marker = self.fixture.root / "cleanup-must-not-run"
        self.task["cleanup"] = plan("cleanup")
        self.task["cleanup"]["hooks"][0]["command"] = ["touch", str(marker)]
        write_record(self.record, self.task)
        result = self.close()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertIn({"action": "close-workspace", "target": self.task["id"], "status": "uncertain"}, result[1]["progress"])
        self.assertEqual(effects, [{"workspace_id": "owned"}])
        self.assertEqual([call["method"] for call in calls], ["workspace.list", "workspace.close"])
        task = json.loads(self.record.read_text())
        self.assertEqual((task["status"], task["cleanup"]["status"]), ("preparing", "queued"))
        receipt = json.loads(self.receipt.read_text())
        self.assertEqual(receipt["status"], "incomplete")
        self.assertIsNone(receipt["runId"])
        self.assertFalse(marker.exists())
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(errors)
