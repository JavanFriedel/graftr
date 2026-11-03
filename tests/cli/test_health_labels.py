import fcntl
import hashlib
import json
from pathlib import Path
import socket
import subprocess
import threading
import time

from test_cli import NativeCase
from status_fixture import write_record, receipt_path
import bench


class HealthLabelTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        record = self.fixture.state / "bench--sample.json"
        return json.loads(record.read_text()), record

    def checks(self, checks):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["healthChecks"] = checks
        path.write_text(json.dumps(config))

    def health(self, task, **env):
        expected = self.invoke(self.reference, ["health", task["id"], "--json"], **env)
        before = bench.readonly_snapshot(self.fixture)
        actual = self.invoke([str(self.native)], ["health", task["id"], "--json"], **env)
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        for result in [expected, actual]:
            data = result[1]["data"]
            if data is not None:
                self.assertRegex(data["health"]["checkedAt"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$")
                data["health"]["checkedAt"] = "<clock>"
                for check in data["health"]["checks"]:
                    self.assertGreaterEqual(check["elapsedMs"], 0)
                    self.assertLess(check["elapsedMs"], 5000)
                    check["elapsedMs"] = "<duration>"
        self.assertEqual(actual, expected)
        return actual

    def network(self, response):
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server.bind(("127.0.0.1", 0))
        server.listen()
        server.settimeout(5)
        port = server.getsockname()[1]
        calls, errors = [], []
        def work():
            try:
                for _ in range(2):
                    connection, _ = server.accept()
                    with connection:
                        connection.settimeout(2)
                        if response is not None:
                            request = bytearray()
                            while not request.endswith(b"\r\n\r\n"):
                                chunk = connection.recv(1024)
                                if not chunk:
                                    break
                                request.extend(chunk)
                            calls.append(bytes(request))
                            if isinstance(response, float):
                                time.sleep(response)
                            else:
                                connection.sendall(response)
                        else:
                            calls.append(b"tcp")
            except Exception as error:
                errors.append(error)
            finally:
                server.close()
        thread = threading.Thread(target=work)
        thread.start()
        self.addCleanup(lambda: thread.join(timeout=6))
        return port, calls, errors

    def test_unconfigured_health_needs_no_lock_checkout_commands_or_herdr(self):
        task, _ = self.managed()
        locks = list(self.fixture.state.glob("setup-*.lock"))
        for lock in locks:
            lock.unlink()  # Source creation is complete; isolate this read-only check.
        result = self.health(task)
        self.assertEqual(result[1]["data"]["health"]["status"], "not-configured")
        self.assertFalse(list(self.fixture.state.glob("setup-*.lock")))
        for columns in [24, 40, 80]:
            self.compare(["health", task["id"]], COLUMNS=str(columns))

    def test_command_health_uses_literal_argv_checkout_overrides_and_private_stdio(self):
        task, _ = self.managed()
        self.checks([
            {"name": "context", "type": "command", "command": ["sh", "-c", 'test "$PORT" = "$1" && test "$PWD" = "$2"', "probe", str(task["localPort"]), task["path"]]},
            {"name": "literal", "type": "command", "command": ["test", "$(touch PWN)", "=", "$(touch PWN)"]},
            {"name": "failed", "type": "command", "command": ["sh", "-c", "printf synthetic-secret; printf private-error >&2; exit 7"]},
            {"name": "signalled", "type": "command", "command": ["sh", "-c", "kill -TERM $$"]},
            {"name": "missing", "type": "command", "command": ["graftr-synthetic-missing-executable"]},
            {"name": "timeout", "type": "command", "command": ["sh", "-c", "sleep 60"], "timeoutMs": 100},
        ])
        result = self.health(task)
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["error"]["code"], "HEALTH_UNHEALTHY")
        self.assertNotIn("synthetic-secret", str(result))
        self.assertNotIn("private-error", str(result))
        self.assertFalse((Path(task["path"]) / "PWN").exists())

    def test_http_tcp_redirect_malformed_and_absolute_timeout(self):
        task, record = self.managed()
        cases = [("tcp", None), ("http", b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"),
                 ("http", b"HTTP/1.1 302 Found\r\nLocation: http://example.invalid/never-follow\r\n\r\n"),
                 ("http", b"not-http\r\n\r\n"), ("http", .2),
                 ("http", b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 204 No Content\r\n\r\n")]
        for kind, response in cases:
            port, calls, errors = self.network(response)
            task["localPort"] = port
            task["overrides"]["PORT"] = str(port)
            write_record(record, task)
            check = {"name": "probe", "type": kind, "timeoutMs": 100}
            if kind == "http":
                check.update(path="/health?literal=%20", expectedStatus=204)
            self.checks([check])
            self.health(task, HTTP_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1")
            if isinstance(response, float):
                # The fixture finishes sleeping after the deadline was already observed.
                pass
            else:
                self.assertEqual(len(calls), 2)
            self.assertFalse(errors)
            if kind == "http":
                self.assertTrue(all(call.startswith(b"GET /health?literal=%20 HTTP/1.1\r\n") for call in calls))

    def test_worktree_lock_interoperates_with_flock_and_blocks_probes(self):
        task, _ = self.managed()
        self.checks([{"name": "sentinel", "type": "command", "command": ["touch", "PWN"]}])
        key = hashlib.sha256(task["path"].encode()).hexdigest()
        lock = self.fixture.state / f"setup-{key}.lock"
        lock.touch(mode=0o600)
        with lock.open("w") as held:
            fcntl.flock(held, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(self.compare(["health", task["id"], "--json"])[0], 1)
        self.assertFalse((Path(task["path"]) / "PWN").exists())
        lock.chmod(0o644)
        self.compare(["health", task["id"], "--json"])
        lock.unlink()
        lock.symlink_to("/dev/null")
        # Both refuse unsafe locks; Node's platform-specific ELOOP text is intentionally not reproduced.
        for command in [self.reference, [str(self.native)]]:
            result = self.invoke(command, ["health", task["id"], "--json"])
            self.assertEqual(result[0], 1)
        self.assertFalse((Path(task["path"]) / "PWN").exists())

    def test_health_refuses_changed_checkout_identity_before_running_commands(self):
        task, record = self.managed()
        self.checks([{"name": "sentinel", "type": "command", "command": ["touch", "PWN"]}])
        subprocess.run(["git", "-C", task["path"], "checkout", "-b", "changed"], env=self.fixture.env, capture_output=True, check=True)
        self.health(task)
        self.assertFalse((Path(task["path"]) / "PWN").exists())
        link = self.fixture.root / "symlink-checkout"
        link.symlink_to(task["path"])
        task["path"] = str(link)
        write_record(record, task)
        self.health(task)

    def attach(self, task, record, socket_path):
        task.update(socket=socket_path, workspaceId="owned")
        write_record(record, task)
        for path in (self.fixture.state / "setup").glob("*.json"):
            receipt = json.loads(path.read_text())
            path.unlink()
            receipt.update(socket=socket_path, workspaceId="owned")
            write_record(receipt_path(self.fixture.state, receipt), receipt)

    def test_label_refuses_standalone_and_pending_setup_without_socket_requests(self):
        task, record = self.managed()
        self.compare(["label", task["id"], "--json"])
        self.attach(task, record, "/synthetic/unavailable.sock")
        receipt_file = next((self.fixture.state / "setup").glob("*.json"))
        receipt = json.loads(receipt_file.read_text())
        receipt.update(status="pending", step="dependencies", dependencies="pending")
        write_record(receipt_file, receipt)
        self.compare(["label", task["id"], "--json"])

    def test_label_refresh_uses_current_branch_rechecks_ownership_and_records_progress(self):
        task, record = self.managed()
        subprocess.run(["git", "-C", task["path"], "checkout", "-b", "feature/new-branch"], env=self.fixture.env, capture_output=True, check=True)
        wanted = f'{task["localPort"]} new branch'
        requests = []
        def respond(request):
            requests.append(request["method"])
            offset = (len(requests) - 1) % 3
            if request["method"] == "workspace.rename":
                self.assertEqual(request["params"], {"workspace_id": "owned", "label": wanted})
                return {"id": request["id"], "result": {}}
            label = "Old label" if offset == 0 else wanted
            return {"id": request["id"], "result": {"workspaces": [{"workspace_id": "owned", "label": label, "worktree": {"checkout_path": task["path"]}}]}}
        socket_path, calls, errors = self.serve(respond, count=6)
        self.attach(task, record, socket_path)
        expected = self.invoke(self.reference, ["label", task["id"], "--json"])
        before = bench.readonly_snapshot(self.fixture)
        actual = self.invoke([str(self.native)], ["label", task["id"], "--json"])
        self.assertEqual(actual, expected)
        self.assertEqual(actual[1]["data"]["branch"], "feature/new-branch")
        self.assertEqual(actual[1]["data"]["recordedBranch"], "task/sample")
        self.assertEqual(actual[1]["progress"], [{"action": "refresh-label", "target": task["id"], "status": "completed"}])
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        self.assertEqual(len(calls), 6)
        self.assertFalse(errors)

    def test_uncertain_rename_keeps_progress_and_does_not_rebind_or_retry(self):
        task, record = self.managed()
        def respond(request):
            if request["method"] == "workspace.rename":
                return None
            return {"id": request["id"], "result": {"workspaces": [{"workspace_id": "owned", "label": "Old", "worktree": {"checkout_path": task["path"]}}]}}
        socket_path, calls, errors = self.serve(respond, count=4)
        self.attach(task, record, socket_path)
        self.invoke(self.reference, ["label", task["id"], "--json"])
        result = self.invoke([str(self.native)], ["label", task["id"], "--json"])
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["progress"], [{"action": "refresh-label", "target": task["id"], "status": "uncertain"}])
        self.assertEqual([call["method"] for call in calls], ["workspace.list", "workspace.rename"] * 2)
        self.assertFalse(errors)
