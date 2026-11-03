import copy
import json
from pathlib import Path
import shutil
import socket

from test_cli import NativeCase
from status_fixture import write_record, plan


class TailscaleTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        path = self.fixture.state / "bench--sample.json"
        task = json.loads(path.read_text())
        with socket.socket() as available:
            available.bind(("127.0.0.1", 0))
            secure = available.getsockname()[1]
        task.update(httpsPort=secure, host="fixture.node.ts.net", url=f"https://fixture.node.ts.net:{secure}")
        task["envOverrides"]["BASE_URL"] = "{{url}}"
        task["overrides"]["BASE_URL"] = task["url"]
        task["cleanup"] = plan("cleanup")
        write_record(path, task)
        directory = self.fixture.root / "bin"
        directory.mkdir(exist_ok=True)
        executable = directory / "tailscale"
        executable.write_text('''#!/usr/bin/python3
import json, os, sys
from pathlib import Path
path = Path(os.environ["MOCK_TAILSCALE"])
state = json.loads(path.read_text())
args = sys.argv[1:]
state.setdefault("calls", []).append(args)
if args == ["status", "--json"]:
    print(json.dumps({"BackendState": "Running", "Self": {"DNSName": state.get("host", "fixture.node.ts.net.")}}))
elif args == ["serve", "status", "--json"]:
    print(json.dumps(state["serve"]))
elif args[0] == "serve":
    secure = next(arg.split("=", 1)[1] for arg in args if arg.startswith("--https="))
    if args[-1] == "off":
        state["serve"] = {}
    else:
        state["serve"] = {"TCP": {secure: {"HTTPS": True}}, "Web": {"fixture.node.ts.net:" + secure: {"Handlers": {"/": {"Proxy": args[-1]}}}}}
    path.write_text(json.dumps(state))
    if state.get("lose_reply"):
        print("synthetic-private-error", file=sys.stderr)
        sys.exit(7)
else:
    sys.exit(9)
path.write_text(json.dumps(state))
''')
        executable.chmod(0o700)
        self.mock = self.fixture.root / "tmp/tailscale-state.json"
        self.env = {"PATH": f"{directory}:/usr/bin:/bin", "MOCK_TAILSCALE": str(self.mock),
                    "GRAFTR_TASK_ID": task["id"], "GRAFTR_CLEANUP_RUN_ID": task["cleanup"]["runId"],
                    "GRAFTR_WORKTREE_PATH": task["path"], "GRAFTR_LOCAL_PORT": str(task["localPort"]),
                    "GRAFTR_HTTPS_PORT": str(secure), "GRAFTR_URL": task["url"]}
        return task

    def receipt(self):
        files = list((self.fixture.state / "tailscale-hooks").glob("*/*.json"))
        return json.loads(files[0].read_text()) if files else None

    def pair(self, phase, config=None, receipt=None, **state):
        results, receipts, calls = [], [], []
        for command in [self.reference, [str(self.native)]]:
            root = self.fixture.state / "tailscale-hooks"
            if root.exists():
                for file in root.glob("*/*.json"):
                    file.unlink()
            if receipt:
                import hashlib
                key = hashlib.sha256(json.dumps([receipt["id"], self.env["GRAFTR_CLEANUP_RUN_ID"]], separators=(",", ":")).encode()).hexdigest()
                write_record(root / key / (receipt["id"] + ".json"), receipt)
                root.chmod(0o700)
            self.mock.write_text(json.dumps({"serve": config or {}, **state}))
            results.append(self.invoke(command, ["hook", "tailscale"], GRAFTR_HOOK_PHASE=phase, **self.env))
            receipts.append(self.receipt())
            calls.append(json.loads(self.mock.read_text()).get("calls", []))
        self.assertEqual(results[0], results[1])
        self.assertEqual(receipts[0], receipts[1])
        self.assertEqual(calls[0], calls[1])
        return results[1], receipts[1], calls[1]

    def route(self, task, target=None, **extra):
        secure = str(task["httpsPort"])
        return {"TCP": {secure: {"HTTPS": True}}, "Web": {f'{task["host"]}:{secure}': {"Handlers": {"/": {"Proxy": target or f'http://127.0.0.1:{task["localPort"]}'}}}}, **extra}

    def test_publish_reuses_only_owned_route_and_cleanup_removes_only_exact_endpoint(self):
        task = self.managed()
        result, receipt, calls = self.pair("afterDependencies")
        self.assertEqual(result, (0, "", ""))
        self.assertEqual(receipt["route"], "active")
        self.assertEqual(sum(call[0] == "serve" and "--bg" in call for call in calls), 1)
        result, _, calls = self.pair("afterDependencies", self.route(task), receipt)
        self.assertEqual(result[0], 0)
        self.assertFalse(any("--bg" in call for call in calls))
        result, receipt, calls = self.pair("cleanup", self.route(task), receipt)
        self.assertEqual(result[0], 0)
        self.assertEqual(receipt["route"], "none")
        self.assertEqual(sum(call[-1] == "off" for call in calls), 1)

    def test_foreign_matching_foreground_and_funnel_routes_are_preserved(self):
        task = self.managed()
        for config in [self.route(task), {"Foreground": {"foreign": self.route(task)}},
                       self.route(task, AllowFunnel={f'{task["host"]}:{task["httpsPort"]}': True})]:
            result, receipt, calls = self.pair("afterDependencies", config)
            self.assertEqual(result[0], 1)
            self.assertEqual(receipt["route"], "none")
            self.assertFalse(any("--bg" in call or call[-1] == "off" for call in calls))

    def test_lost_publish_reply_keeps_pending_identity_and_explicit_retry_verifies_effect(self):
        task = self.managed()
        result, receipt, _ = self.pair("afterDependencies", lose_reply=True)
        self.assertEqual(result[0], 1)
        self.assertEqual(receipt["route"], "pending")
        self.assertNotIn("synthetic-private-error", str(result))
        result, receipt, calls = self.pair("afterDependencies", self.route(task), receipt)
        self.assertEqual(result[0], 0)
        self.assertEqual(receipt["route"], "active")
        self.assertFalse(any("--bg" in call for call in calls))

    def test_cleanup_without_publication_is_quiet_and_changed_external_endpoint_is_kept(self):
        task = self.managed()
        result, receipt, calls = self.pair("cleanup")
        self.assertEqual(result[0], 0)
        self.assertIsNone(receipt)
        self.assertEqual(calls, [])
        _, receipt, _ = self.pair("afterDependencies")
        result, after, calls = self.pair("cleanup", self.route(task, target="http://127.0.0.1:1"), receipt)
        self.assertEqual(result[0], 1)
        self.assertEqual(after, receipt)
        self.assertFalse(any(call[-1] == "off" for call in calls))

    def test_busy_port_wrong_host_and_stale_context_never_publish(self):
        task = self.managed()
        with socket.socket() as listener:
            listener.bind(("0.0.0.0", task["httpsPort"]))
            listener.listen()
            result, _, calls = self.pair("afterDependencies")
            self.assertEqual(result[0], 1)
            self.assertFalse(any("--bg" in call for call in calls))
        result, _, calls = self.pair("afterDependencies", host="different.node.ts.net.")
        self.assertEqual(result[0], 1)
        self.assertFalse(any("--bg" in call for call in calls))
        for key in ["GRAFTR_CLEANUP_RUN_ID", "GRAFTR_WORKTREE_PATH", "GRAFTR_LOCAL_PORT", "GRAFTR_URL"]:
            env = {**self.env, key: "stale-context", "GRAFTR_HOOK_PHASE": "afterDependencies"}
            self.compare(["hook", "tailscale"], **env)
