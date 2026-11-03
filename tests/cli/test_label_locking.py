import fcntl
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
from status_fixture import receipt_path, write_record
import bench


class LabelLockingTests(NativeCase):
    def prepare(self):
        if hasattr(self, "server"):
            self.server.close()
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"])
        self.assertEqual(result[0], 0, result)
        self.server = LifecycleHerdr(self.fixture)
        self.server.open = True
        self.server.label = "Old label"
        self.addCleanup(self.server.close)
        record = self.fixture.state / "bench--sample.json"
        task = json.loads(record.read_text())
        task.update(socket=self.server.path, workspaceId="owned")
        write_record(record, task)
        old_receipt = next((self.fixture.state / "setup").glob("*.json"))
        receipt = json.loads(old_receipt.read_text())
        old_receipt.unlink()
        receipt.update(socket=self.server.path, workspaceId="owned")
        self.receipt = write_record(receipt_path(self.fixture.state, receipt), receipt)
        self.task, self.record = task, record
        self.wanted = f'{task["localPort"]} sample'

    def registry_lock(self):
        return (self.fixture.state / "operation.lock").open("r+")

    def change(self, kind):
        if kind == "task":
            write_record(self.record, {**self.task, "status": "closed"})
        elif kind == "receipt":
            receipt = json.loads(self.receipt.read_text())
            write_record(self.receipt, {**receipt, "message": "A newer setup observation"})
        elif kind == "profile":
            path = self.fixture.root / "config/graftr/projects.json"
            config = json.loads(path.read_text())
            config["projects"]["bench"]["workspaceLabel"] = "Changed {{taskName}}"
            path.write_text(json.dumps(config))
        elif kind == "conflict":
            write_record(self.fixture.state / "bench--conflicting.json", {**self.task, "id": "bench--conflicting"})
        elif kind == "branch":
            self.fixture.git("-C", self.task["path"], "checkout", "-qb", "feature/concurrent")
        elif kind == "removed":
            self.record.unlink()
        else:
            self.fail(f"Unexpected change: {kind}")

    def test_inventory_reads_release_registry_lock_but_rename_and_worktree_remain_guarded(self):
        self.prepare()
        observations = []
        tree = self.fixture.state / f'setup-{hashlib.sha256(self.task["path"].encode()).hexdigest()}.lock'
        response = self.server.response
        def probe(method, params):
            available = []
            for path in (self.fixture.state / "operation.lock", tree):
                with path.open("r+") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        available.append(True)
                    except BlockingIOError:
                        available.append(False)
            observations.append((method, *available))
            return response(method, params)
        self.server.response = probe
        before = bench.readonly_snapshot(self.fixture)
        result = self.invoke([str(self.native)], ["label", self.task["id"], "--json"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual(observations, [("workspace.list", True, False), ("workspace.rename", False, False),
                                        ("workspace.list", True, False)])
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        self.assertEqual(self.server.label, self.wanted)
        self.assertFalse(self.server.errors)

    def test_unrelated_creation_can_finish_while_label_inventory_is_pending(self):
        self.prepare()
        response = self.server.response
        creations = []
        def create(method, params):
            if method == "workspace.list" and not creations:
                child = subprocess.run([str(self.native), "create", "bench", "other", "--no-herdr", "--json"],
                                       cwd=self.fixture.repo, env=self.fixture.env, stdin=subprocess.DEVNULL,
                                       capture_output=True, text=True, timeout=3)
                creations.append((child.returncode, json.loads(child.stdout), child.stderr))
            return response(method, params)
        self.server.response = create
        result = self.invoke([str(self.native)], ["label", self.task["id"], "--json"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["data"]["status"], "refreshed")
        self.assertEqual(len(creations), 1)
        self.assertEqual((creations[0][0], creations[0][2]), (0, ""), creations)
        self.assertEqual(creations[0][1]["data"]["status"], "ready")
        self.assertTrue((self.fixture.state / "bench--other.json").is_file())
        self.assertEqual(self.server.label, self.wanted)
        self.assertFalse(self.server.errors)

    def context_race(self, kind, listing):
        response = self.server.response
        inventories = 0
        saved = []
        def change(method, params):
            nonlocal inventories
            if method == "workspace.list":
                inventories += 1
                if inventories == listing:
                    with self.registry_lock() as lock:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        self.change(kind)
                    saved.append(bench.readonly_snapshot(self.fixture))
            return response(method, params)
        self.server.response = change
        result = self.invoke([str(self.native)], ["label", self.task["id"], "--json"])
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial" if listing == 2 else "failed")
        self.assertIsNone(result[1]["data"])
        self.assertEqual(len(saved), 1)
        self.assertEqual(bench.readonly_snapshot(self.fixture), saved[0])
        self.assertEqual([call["method"] for call in self.server.calls],
                         ["workspace.list", "workspace.rename", "workspace.list"] if listing == 2 else ["workspace.list"])
        self.assertEqual(result[1]["progress"],
                         [{"action": "refresh-label", "target": self.task["id"], "status": "completed"}] if listing == 2 else [])
        self.assertEqual(self.server.label, self.wanted if listing == 2 else "Old label")
        if kind == "branch":
            self.assertEqual(self.fixture.git("-C", self.task["path"], "symbolic-ref", "--short", "HEAD").strip(), "feature/concurrent")
        self.assertFalse(self.server.errors)

    def test_context_changes_while_reading_inventory_refuse_before_rename(self):
        for kind in ("task", "receipt", "profile", "conflict", "branch", "removed"):
            with self.subTest(kind=kind):
                self.prepare()
                self.context_race(kind, 1)

    def test_context_changes_during_final_verification_report_partial_without_retry(self):
        for kind in ("task", "receipt", "profile", "conflict", "branch", "removed"):
            with self.subTest(kind=kind):
                self.prepare()
                self.context_race(kind, 2)

    def test_unchanged_label_is_rechecked_without_renaming(self):
        for changed in (False, True):
            with self.subTest(changed=changed):
                self.prepare()
                self.server.label = self.wanted
                response = self.server.response
                def change(method, params):
                    if changed:
                        with self.registry_lock() as lock:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                            self.change("receipt")
                    return response(method, params)
                self.server.response = change
                result = self.invoke([str(self.native)], ["label", self.task["id"], "--json"])
                self.assertEqual(result[0], 1 if changed else 0, result)
                self.assertEqual(result[1]["outcome"], "failed" if changed else "completed")
                if not changed:
                    self.assertEqual(result[1]["data"]["status"], "unchanged")
                self.assertEqual(result[1]["progress"], [])
                self.assertEqual([call["method"] for call in self.server.calls], ["workspace.list"])
                self.assertFalse(self.server.errors)

    def test_inventory_identity_and_unique_workspace_checks_still_refuse_rename(self):
        for listing in (1, 2):
            for kind in ("wrong-path", "duplicate-id", "duplicate-path"):
                with self.subTest(kind=kind, listing=listing):
                    self.prepare()
                    response = self.server.response
                    inventories = 0
                    def drift(method, params):
                        nonlocal inventories
                        result = response(method, params)
                        if method != "workspace.list":
                            return result
                        inventories += 1
                        if inventories != listing:
                            return result
                        spaces = result["workspaces"]
                        if kind == "wrong-path":
                            spaces[-1]["worktree"]["checkout_path"] = str(self.fixture.repo)
                        elif kind == "duplicate-id":
                            spaces.append(self.server.workspace())
                        else:
                            other = self.server.workspace()
                            other["workspace_id"] = "conflicting"
                            spaces.append(other)
                        return result
                    self.server.response = drift
                    result = self.invoke([str(self.native)], ["label", self.task["id"], "--json"])
                    self.assertEqual(result[0], 1, result)
                    self.assertEqual(result[1]["outcome"], "partial" if listing == 2 else "failed")
                    self.assertEqual(result[1]["progress"],
                                     [{"action": "refresh-label", "target": self.task["id"], "status": "completed"}] if listing == 2 else [])
                    self.assertEqual([call["method"] for call in self.server.calls],
                                     ["workspace.list", "workspace.rename", "workspace.list"] if listing == 2 else ["workspace.list"])
                    self.assertFalse(self.server.errors)
