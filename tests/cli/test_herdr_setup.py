import copy
import json
from pathlib import Path
import socket
import threading

from test_cli import NativeCase
from status_fixture import write_record
import bench


class HerdrFixture:
    def __init__(self, path, checkout):
        self.path, self.checkout = str(path), str(checkout)
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(self.path)
        self.server.listen()
        self.server.settimeout(5)
        self.errors = []
        self.reset()
        self.thread = threading.Thread(target=self.work)
        self.thread.start()

    def reset(self, close_at=None, dispatch=True):
        self.calls = []
        self.label = "Unlabeled"
        self.tabs = [{"tab_id": "initial", "label": "1", "pane_count": 1}]
        self.inventories = 0
        self.close_at, self.dispatch = close_at, dispatch

    def work(self):
        try:
            while True:
                connection, _ = self.server.accept()
                with connection:
                    line = connection.makefile("rb").readline()
                    if not line:
                        break
                    request = json.loads(line)
                    method, params = request["method"], request["params"]
                    self.calls.append({"method": method, "params": params})
                    if method == "workspace.list":
                        self.inventories += 1
                        spaces = [] if self.close_at and self.inventories >= self.close_at else [{"workspace_id": "owned", "label": self.label, "worktree": {"checkout_path": self.checkout}}]
                        result = {"workspaces": spaces}
                    elif method == "pane.current":
                        if params != {"caller_pane_id": "explicit-caller"}:
                            raise AssertionError(params)
                        result = {"pane": {"workspace_id": "owned"}}
                    elif method == "workspace.rename":
                        if params["workspace_id"] != "owned":
                            raise AssertionError(params)
                        self.label = params["label"]
                        result = {}
                    elif method == "tab.list":
                        result = {"tabs": self.tabs}
                    elif method == "tab.rename":
                        next(tab for tab in self.tabs if tab["tab_id"] == params["tab_id"])["label"] = params["label"]
                        result = {}
                    elif method == "tab.create":
                        if params["workspace_id"] != "owned" or params["cwd"] != self.checkout or params["focus"] is not False:
                            raise AssertionError(params)
                        tab = {"tab_id": f'tab-{len(self.tabs)}', "label": params["label"], "pane_count": 1}
                        self.tabs.append(tab)
                        result = {"tab": tab}
                    elif method == "plugin.action.invoke":
                        result = {"type": "plugin_action_invoked" if self.dispatch else "not-accepted", "log": {"status": "running", "log_id": "fixture-worker"}}
                    elif method == "notification.show":
                        result = {}
                    else:
                        raise AssertionError(f"Unexpected mutation or focused fallback: {request}")
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


class HerdrSetupTests(NativeCase):
    def managed(self, source_env=True):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        self.record_path = self.fixture.state / "bench--sample.json"
        self.receipt_path = next((self.fixture.state / "setup").glob("*.json"))
        self.initial_task = json.loads(self.record_path.read_text())
        self.initial_receipt = json.loads(self.receipt_path.read_text())
        config_path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(config_path.read_text())
        config["herdr"] = "auto"
        if not source_env:
            config["projects"]["bench"].pop("envSource")
        config_path.write_text(json.dumps(config))
        self.server = HerdrFixture(self.fixture.root / "setup.sock", self.fixture.checkout)
        self.addCleanup(self.server.close)
        self.env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": self.server.path, "HERDR_WORKSPACE_ID": "stale", "HERDR_PANE_ID": "explicit-caller"}

    def reset(self, **server):
        write_record(self.record_path, self.initial_task)
        write_record(self.receipt_path, self.initial_receipt)
        self.server.reset(**server)

    def pair(self, args, **server):
        results, calls, tasks = [], [], []
        for command in [self.reference, [str(self.native)]]:
            self.reset(**server)
            result = self.invoke(command, ["setup", *args, "--json"], **self.env)
            results.append(bench.normalize(list(result), self.fixture.root))
            calls.append(bench.normalize(copy.deepcopy(self.server.calls), self.fixture.root))
            tasks.append(bench.normalize(json.loads(self.record_path.read_text()), self.fixture.root))
        self.assertEqual(results[0], results[1])
        self.assertEqual(calls[0], calls[1])
        self.assertEqual(tasks[0], tasks[1])
        self.assertFalse(self.server.errors)
        return results[1], calls[1], tasks[1]

    def test_synchronous_attachment_uses_explicit_caller_and_reuses_initial_shell_without_focus(self):
        self.managed()
        result, calls, task = self.pair(["--wait"])
        self.assertEqual(result[0], 0)
        self.assertEqual(task["workspaceId"], "owned")
        self.assertEqual(task["tabs"], {"agent": "initial", "editor": "tab-1", "terminal": "tab-2", "server": "tab-3"})
        self.assertEqual(sum(call["method"] == "tab.create" for call in calls), 3)
        self.assertFalse(any("synthetic-initial" in str(call) for call in calls))
        self.assertFalse(any("focus" in call["method"] for call in calls))

    def test_without_dotenv_existing_native_shell_is_preserved_and_new_tabs_get_only_public_overrides(self):
        self.managed(source_env=False)
        result, calls, task = self.pair(["--wait"])
        self.assertEqual(result[0], 0)
        self.assertNotEqual(task["tabs"]["agent"], "initial")
        self.assertEqual(sum(call["method"] == "tab.create" for call in calls), 4)
        self.assertTrue(all(call["params"]["env"] == {"PORT": "40000"} for call in calls if call["method"] == "tab.create"))

    def test_dispatch_is_queued_and_selected_worker_consumes_the_same_attempt(self):
        self.managed()
        queues, workers, calls = [], [], []
        for command in [self.reference, [str(self.native)]]:
            self.reset()
            queued = self.invoke(command, ["setup", "--json"], **self.env)
            self.assertEqual(queued[1]["outcome"], "queued")
            self.assertEqual(queued[1]["data"]["logId"], "fixture-worker")
            self.assertFalse(any(call["method"] == "tab.create" for call in self.server.calls))
            receipt = json.loads(self.receipt_path.read_text())
            worker = self.invoke(command, ["setup", "--wait", "--json"], **{**self.env, "HERDR_WORKSPACE_ID": "owned",
                "HERDR_PLUGIN_ACTION_ID": "setup", "HERDR_PLUGIN_ROOT": str(self.fixture.repo),
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps({"invocation_source": "graftr-queued-setup", "correlation_id": receipt["runId"]})})
            queues.append(bench.normalize(list(queued), self.fixture.root))
            workers.append(bench.normalize(list(worker), self.fixture.root))
            calls.append(bench.normalize(copy.deepcopy(self.server.calls), self.fixture.root))
            self.assertEqual(worker[1]["data"]["status"], "ready")
            self.assertEqual(json.loads(self.receipt_path.read_text())["runId"], receipt["runId"])
        self.assertEqual(queues[0], queues[1])
        self.assertEqual(workers[0], workers[1])
        self.assertEqual(calls[0], calls[1])
        self.assertFalse(self.server.errors)

    def test_workspace_disappearance_fences_setup_without_restoring_tabs_or_readiness(self):
        self.managed()
        result, calls, task = self.pair(["--wait"], close_at=5)
        self.assertEqual(result[0], 1)
        self.assertEqual(task["status"], "closed")
        self.assertFalse(any(call["method"] == "tab.create" for call in calls))
        self.assertFalse(any(call["method"] == "notification.show" for call in calls))
        self.assertIsNone(json.loads(self.receipt_path.read_text())["runId"])

    def test_unaccepted_dispatch_is_retained_and_attention_is_best_effort_not_a_setup_replay(self):
        self.managed()
        result, calls, task = self.pair([], dispatch=False)
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["data"]["status"], "incomplete")
        self.assertTrue(any(call["method"] == "notification.show" for call in calls))
        self.assertEqual(sum(call["method"] == "plugin.action.invoke" for call in calls), 1)
        self.assertFalse(any(call["method"] == "tab.create" for call in calls))

    def test_legacy_queued_receipt_without_timings_finishes_without_inserting_invalid_null(self):
        self.managed()
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.reset()
            queued = self.invoke(command, ["setup", "--json"], **self.env)
            self.assertEqual(queued[0], 0)
            receipt = json.loads(self.receipt_path.read_text())
            receipt.pop("timings")
            write_record(self.receipt_path, receipt)
            worker = self.invoke(command, ["setup", "--wait", "--json"], **{**self.env,
                "HERDR_WORKSPACE_ID": "owned", "HERDR_PLUGIN_ACTION_ID": "setup", "HERDR_PLUGIN_ROOT": str(self.fixture.repo),
                "HERDR_PLUGIN_CONTEXT_JSON": json.dumps({"invocation_source": "graftr-queued-setup", "correlation_id": receipt["runId"]})})
            self.assertEqual(worker[0], 0, worker)
            self.assertNotIn("timings", worker[1]["data"])
            self.assertNotIn("timings", json.loads(self.receipt_path.read_text()))
            results.append(bench.normalize(list(worker), self.fixture.root))
        self.assertEqual(results[0], results[1])
