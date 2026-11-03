import copy
import json
from pathlib import Path
import shutil
import socket
import threading

from test_cli import NativeCase
from status_fixture import write_record
import bench


class LifecycleHerdr:
    """Only fixture-owned Git checkouts and a local synthetic RPC server."""
    def __init__(self, fixture, associated=True):
        self.fixture = fixture
        self.path = str(fixture.root / "lifecycle.sock")
        self.associated, self.open = associated, False
        self.calls, self.errors, self.tabs = [], [], []
        self.label = "sample"
        self.caller = "parent"
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.path)
        self.server.listen()
        self.server.settimeout(5)
        self.thread = threading.Thread(target=self.work)
        self.thread.start()

    def workspace(self, parent=False):
        return {"workspace_id": "parent" if parent else "owned", "label": "Parent" if parent else self.label,
                "worktree": {"checkout_path": str(self.fixture.repo if parent else self.fixture.checkout)}}

    def response(self, method, p):
        if method == "workspace.list":
            return {"workspaces": ([self.workspace(True)] if self.associated else []) + ([self.workspace()] if self.open else [])}
        if method == "pane.current":
            assert p == {"caller_pane_id": "explicit-caller"}, p
            return {"pane": {"workspace_id": self.caller}}
        if method == "worktree.list":
            return {"source": {"source_workspace_id": "parent", "repo_root": str(self.fixture.repo), "source_checkout_path": str(self.fixture.repo)}}
        if method in ["worktree.create", "worktree.open"]:
            assert p["workspace_id"] == "parent" and p["focus"] is False, p
            if p["path"] == str(self.fixture.repo):
                self.associated = True
                return {"type": "worktree_opened", "already_open": True, "workspace": self.workspace(True), "tab": {"tab_id": "parent-tab"}}
            assert p["path"] == str(self.fixture.checkout), p
            if method == "worktree.create":
                args = ["worktree", "add"]
                if "base" in p:
                    args += ["-b", p["branch"]]
                self.fixture.git(*args, "--", p["path"], p.get("base", p["branch"]))
            already = self.open
            self.open = True
            self.tabs = [{"tab_id": "initial", "label": "1", "pane_count": 1}]
            return {"type": "worktree_created" if method == "worktree.create" else "worktree_opened", "already_open": already,
                    "workspace": self.workspace(), "tab": {"tab_id": "initial"}}
        if method == "workspace.close":
            assert p == {"workspace_id": "owned"}, p
            self.open = False
            return {}
        if method == "workspace.rename":
            assert p["workspace_id"] == "owned", p
            self.label = p["label"]
            return {}
        if method == "tab.list":
            return {"tabs": self.tabs}
        if method == "tab.rename":
            next(t for t in self.tabs if t["tab_id"] == p["tab_id"])["label"] = p["label"]
            return {}
        if method == "tab.create":
            assert p["workspace_id"] == "owned" and p["focus"] is False and p["cwd"] == str(self.fixture.checkout), p
            tab = {"tab_id": f"tab-{len(self.tabs)}", "label": p["label"], "pane_count": 1}
            self.tabs.append(tab)
            return {"tab": tab}
        if method == "layout.apply":
            assert p["workspace_id"] == "owned" and p["focus"] is False and p["tab_label"] == "welcome back", p
            self.tabs.append({"tab_id": "welcome", "label": p["tab_label"], "pane_count": 1})
            return {"layout": {"workspace_id": "owned", "tab_id": "welcome"}}
        if method == "tab.move":
            assert p == {"tab_id": "welcome", "insert_index": 0}, p
            self.tabs.insert(0, self.tabs.pop())
            return {"tabs": self.tabs}
        if method == "plugin.action.invoke":
            return {"type": "plugin_action_invoked", "log": {"status": "running", "log_id": "fixture-worker"}}
        if method == "notification.show":
            return {}
        raise AssertionError((method, p))

    def work(self):
        try:
            while True:
                connection, _ = self.server.accept()
                with connection:
                    line = connection.makefile("rb").readline()
                    if not line:
                        break
                    request = json.loads(line)
                    self.calls.append({"method": request["method"], "params": request["params"]})
                    result = self.response(request["method"], request["params"])
                    connection.sendall((json.dumps({"id": request["id"], "result": result}) + "\n").encode())
        except Exception as error:
            self.errors.append(error)
        finally:
            self.server.close()
            Path(self.path).unlink(missing_ok=True)

    def close(self):
        if self.thread.is_alive():
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                    connection.connect(self.path)
            except OSError:
                pass
        self.thread.join(timeout=6)


class HerdrLifecycleTests(NativeCase):
    def fresh(self):
        if hasattr(self, "server"):
            self.server.close()
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def configure(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        value = json.loads(path.read_text())
        value.update(herdr="auto", **changes)
        path.write_text(json.dumps(value))
        return path

    def herdr(self, associated=True):
        self.server = LifecycleHerdr(self.fixture, associated)
        self.addCleanup(self.server.close)
        self.env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": self.server.path, "HERDR_PANE_ID": "explicit-caller"}

    def normalize_calls(self, calls):
        value = copy.deepcopy(calls)
        for call in value:
            if call["method"] == "layout.apply":
                argv = call["params"]["root"]["command"]
                call["params"]["root"]["command"] = ["<native-or-source>", *argv[argv.index("_thaw-message"):]]
        return bench.normalize(value, self.fixture.root)

    def test_create_close_open_and_freeze_thaw_use_owned_workspaces_and_first_welcome_tab(self):
        results, calls = [], []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure()
            self.herdr()
            outputs = []
            for args in [["create", "bench", "sample", "--wait"], ["close", "bench--sample"],
                         ["open", "bench--sample", "--wait"], ["freeze", "bench--sample", "--yes", "--note", "Return after review"],
                         ["thaw", "bench--sample"]]:
                result = self.invoke(command, [*args, "--json"], **self.env)
                self.assertEqual(result[0], 0, result)
                outputs.append(list(result))
            self.assertEqual(self.server.tabs[0]["tab_id"], "welcome")
            self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)
            self.assertEqual((self.fixture.checkout / "node_modules/install-count").read_text().strip(), "1")
            self.assertFalse(self.server.errors)
            results.append(bench.normalize(outputs, self.fixture.root))
            calls.append(self.normalize_calls(self.server.calls))
        self.assertEqual(results[0], results[1])
        self.assertEqual(calls[0], calls[1])

    def test_creation_registers_only_proven_existing_parent_and_never_focuses(self):
        results, calls = [], []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure()
            self.herdr(associated=False)
            result = self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)
            self.assertEqual(result[0], 0, result)
            self.assertTrue(self.server.associated)
            self.assertEqual(sum(c["method"] == "worktree.open" for c in self.server.calls), 1)
            results.append(bench.normalize(list(result), self.fixture.root))
            calls.append(self.normalize_calls(self.server.calls))
            self.assertFalse(self.server.errors)
        self.assertEqual(results[0], results[1])
        self.assertEqual(calls[0], calls[1])

    def test_close_inside_target_hands_off_without_closing_the_caller(self):
        results, calls = [], []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.configure()
            self.herdr()
            self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--wait", "--json"], **self.env)[0], 0)
            self.server.caller = "owned"
            self.server.calls.clear()
            result = self.invoke(command, ["close", "bench--sample", "--json"], **self.env)
            self.assertEqual(result[1]["outcome"], "queued", result)
            self.assertTrue(self.server.open)
            self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))
            results.append(bench.normalize(list(result), self.fixture.root))
            calls.append(self.normalize_calls(self.server.calls))
        self.assertEqual(results[0], results[1])
        self.assertEqual(calls[0], calls[1])
