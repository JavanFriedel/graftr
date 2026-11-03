import fcntl
import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
from status_fixture import STAMP, UUID, receipt_path, setup, write_record
import bench


class CreationDiscoveryTests(NativeCase):
    def prepare(self, associated=True, existing=False):
        if hasattr(self, "server"):
            self.server.close()
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.config = root / "config/graftr/projects.json"
        config = json.loads(self.config.read_text())
        config["herdr"] = "auto"
        self.config.write_text(json.dumps(config))
        if existing:
            self.fixture.git("branch", "feature/retained")
        self.server = LifecycleHerdr(self.fixture, associated=associated)
        self.addCleanup(self.server.close)
        self.env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": self.server.path, "HERDR_PANE_ID": "explicit-caller"}

    def create(self, *args):
        return self.invoke([str(self.native)], ["create", "bench", "sample", "--no-setup", *args, "--json"], **self.env)

    def lock_available(self):
        path = self.fixture.state / "operation.lock"
        if not path.exists():
            return True
        with path.open("r+") as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except BlockingIOError:
                return False

    def test_configured_parent_inventory_is_unlocked_but_checkout_creation_is_serialized(self):
        self.prepare()
        response = self.server.response
        observations = []
        def probe(method, params):
            observations.append((method, self.lock_available()))
            return response(method, params)
        self.server.response = probe
        result = self.create()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(observations, [("workspace.list", True), ("workspace.list", True), ("worktree.create", False)])
        self.assertEqual(result[1]["data"]["status"], "skipped")
        self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
        self.assertFalse(self.server.errors)

    def test_parent_registration_discovery_is_unlocked_but_registration_and_verification_stay_guarded(self):
        self.prepare(associated=False)
        response = self.server.response
        observations = []
        def probe(method, params):
            observations.append((method, self.lock_available()))
            return response(method, params)
        self.server.response = probe
        result = self.create()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(observations, [("workspace.list", True), ("workspace.list", True),
                                        ("pane.current", True), ("worktree.list", True),
                                        ("worktree.open", False), ("workspace.list", False), ("worktree.create", False)])
        self.assertTrue(self.server.associated)
        self.assertFalse(self.server.errors)

    def test_unrelated_creation_can_finish_during_parent_discovery_without_invalidating_the_target(self):
        self.prepare()
        response = self.server.response
        inventories = 0
        creations = []
        def create(method, params):
            nonlocal inventories
            if method == "workspace.list":
                inventories += 1
                if inventories == 2:
                    child = subprocess.run([str(self.native), "create", "bench", "other", "--no-herdr", "--json"],
                                           cwd=self.fixture.repo, env=self.fixture.env, stdin=subprocess.DEVNULL,
                                           capture_output=True, text=True, timeout=3)
                    creations.append((child.returncode, json.loads(child.stdout), child.stderr))
            return response(method, params)
        self.server.response = create
        result = self.create()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(len(creations), 1)
        self.assertEqual((creations[0][0], creations[0][2]), (0, ""), creations)
        self.assertEqual(json.loads((self.fixture.state / "bench--other.json").read_text())["status"], "ready")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse(self.server.errors)

    def reserve(self):
        self.fixture.git("branch", "task/sample")
        record = {"schemaVersion": 1, "id": "bench--sample", "generation": UUID, "status": "frozen", "frozenAt": STAMP,
                  "until": None, "note": None, "skipInstall": False, "attempts": 0, "nextAttemptAt": None,
                  "message": "Reserved during discovery", "thawedAt": None,
                  "removal": {"id": "bench--sample", "repo": str(self.fixture.repo), "path": str(self.fixture.checkout),
                              "branch": "task/sample", "baseCommit": self.fixture.commit, "socket": None,
                              "workspaceId": None, "currentBranch": "task/sample", "currentCommit": self.fixture.commit,
                              "cleanupRunId": UUID}}
        write_record(self.fixture.state / "frozen/bench--sample.json", record)

    def change(self, kind):
        if kind == "frozen":
            self.reserve()
        elif kind == "receipt":
            receipt = setup(None, self.fixture.checkout)
            write_record(receipt_path(self.fixture.state, receipt), receipt)
        elif kind == "profile":
            config = json.loads(self.config.read_text())
            config["projects"]["bench"]["installCommand"] = ["printf", "synthetic-private-discovery"]
            self.config.write_text(json.dumps(config))
        elif kind == "policy":
            config = json.loads(self.config.read_text())
            config["herdr"] = "off"
            self.config.write_text(json.dumps(config))
        elif kind == "task":
            result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"])
            self.assertEqual(result[0], 0, result)
        elif kind == "registration":
            self.fixture.git("worktree", "add", "-qb", "feature/occupied", str(self.fixture.checkout))
        elif kind == "adoption":
            self.fixture.git("worktree", "add", "-qb", "task/sample", str(self.fixture.checkout))
            record = {"schemaVersion": 1, "id": "bench--sample", "project": "bench", "repo": str(self.fixture.repo),
                      "path": str(self.fixture.checkout), "branch": "task/sample", "adoptedAt": STAMP, "socket": None,
                      "workspaceId": None, "localPort": None, "url": None, "environment": "external", "dependencies": "external", "hooks": []}
            write_record(self.fixture.state / "adopted/bench--sample.json", record)
        elif kind == "directory":
            self.fixture.checkout.mkdir(parents=True)
            (self.fixture.checkout / "keep").write_text("existing checkout content")
        elif kind == "symlink":
            outside = self.fixture.root / "outside"
            outside.mkdir()
            self.fixture.checkout.parent.symlink_to(outside)
        elif kind == "branch":
            tree = self.fixture.git("rev-parse", "main^{tree}").strip()
            commit = self.fixture.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit-tree",
                                      tree, "-p", self.fixture.commit, "-m", "Retained branch advanced").strip()
            self.fixture.git("update-ref", "refs/heads/feature/retained", commit, self.fixture.commit)
        else:
            self.fail(f"Unknown change: {kind}")

    def test_discovery_context_changes_refuse_before_registration_intent_or_checkout_mutations(self):
        for kind in ("frozen", "receipt", "profile", "policy", "task", "registration", "adoption", "directory", "symlink", "branch"):
            with self.subTest(kind=kind):
                self.prepare(associated=False, existing=kind == "branch")
                response = self.server.response
                inventories = 0
                saved = []
                def change(method, params):
                    nonlocal inventories
                    if method == "workspace.list":
                        inventories += 1
                        if inventories == 2:
                            with (self.fixture.state / "operation.lock").open("r+") as lock:
                                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                                if kind != "task":
                                    self.change(kind)
                            if kind == "task":
                                self.change(kind)
                            saved.append((bench.readonly_snapshot(self.fixture), self.fixture.git("show-ref"),
                                          self.fixture.git("worktree", "list", "--porcelain")))
                    return response(method, params)
                self.server.response = change
                args = ["--branch", "feature/retained"] if kind == "branch" else []
                result = self.create(*args)
                self.assertEqual(result[0], 1, result)
                self.assertEqual(result[1]["outcome"], "failed")
                self.assertEqual(result[1]["progress"], [])
                self.assertNotIn("synthetic-private-discovery", json.dumps(result))
                self.assertEqual(len(saved), 1)
                self.assertEqual((bench.readonly_snapshot(self.fixture), self.fixture.git("show-ref"),
                                  self.fixture.git("worktree", "list", "--porcelain")), saved[0])
                self.assertFalse(self.server.associated)
                self.assertFalse(self.server.open)
                self.assertFalse(any(call["method"] in ("worktree.open", "worktree.create") for call in self.server.calls))
                self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
                self.assertFalse(self.server.errors)

    def test_invalid_discovery_never_registers_or_creates(self):
        for kind in ("duplicate-parent", "wrong-source", "malformed"):
            with self.subTest(kind=kind):
                self.prepare(associated=kind != "wrong-source")
                response = self.server.response
                inventories = 0
                def invalid(method, params):
                    nonlocal inventories
                    result = response(method, params)
                    if method == "workspace.list":
                        inventories += 1
                        if inventories == 2:
                            if kind == "duplicate-parent":
                                result["workspaces"].append(self.server.workspace(True))
                            elif kind == "malformed":
                                result["workspaces"] = "synthetic-private-discovery"
                    if method == "worktree.list":
                        result["source"]["repo_root"] = "/not-the-requested-repository"
                    return result
                self.server.response = invalid
                result = self.create()
                self.assertEqual(result[0], 1, result)
                self.assertEqual(result[1]["progress"], [])
                self.assertNotIn("synthetic-private-discovery", json.dumps(result))
                self.assertFalse(self.fixture.checkout.exists())
                self.assertNotIn("task/sample", self.fixture.git("branch", "--list"))
                self.assertFalse(any(call["method"] in ("worktree.open", "worktree.create") for call in self.server.calls))
                self.assertFalse(self.server.errors)

    def test_uncertain_parent_registration_is_not_retried_and_never_creates_a_checkout(self):
        self.prepare(associated=False)
        response = self.server.response
        def unusable(method, params):
            result = response(method, params)
            return None if method == "worktree.open" else result
        self.server.response = unusable
        result = self.create()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertEqual(result[1]["progress"], [{"action": "register-parent", "target": str(self.fixture.repo), "status": "uncertain"}])
        self.assertTrue(self.server.associated)
        self.assertFalse(self.fixture.checkout.exists())
        self.assertEqual(sum(call["method"] == "worktree.open" for call in self.server.calls), 1)
        self.assertFalse(any(call["method"] == "worktree.create" for call in self.server.calls))
        self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
        self.assertFalse(self.server.errors)
