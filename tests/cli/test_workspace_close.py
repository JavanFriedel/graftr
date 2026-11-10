import json
import os
import pty
import select
import signal
import subprocess
import time

from test_cli import NativeCase
from test_herdr_lifecycle import LifecycleHerdr
from status_fixture import write_record


class WorkspaceHerdr(LifecycleHerdr):
    def __init__(self, fixture):
        super().__init__(fixture)
        self.parent_path = str(fixture.repo)
        self.parent_label = "Main checkout"
        self.reject_dispatch = False

    def workspace(self, parent=False):
        if parent:
            return {"workspace_id": "parent", "label": self.parent_label,
                    "worktree": {"checkout_path": self.parent_path} if self.parent_path else None}
        return super().workspace(parent)

    def response(self, method, params):
        if method == "workspace.close":
            assert params == {"workspace_id": "parent"}, params
            self.associated = False
            return {}
        if method == "plugin.action.invoke" and self.reject_dispatch:
            return {"type": "plugin_action_invoked", "log": {"status": "failed", "log_id": "failed-worker"}}
        return super().response(method, params)


class WorkspaceCloseTests(NativeCase):
    def setUp(self):
        super().setUp()
        self.server = WorkspaceHerdr(self.fixture)
        self.addCleanup(self.server.close)
        self.env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": self.server.path,
                    "HERDR_PANE_ID": "explicit-caller", "HERDR_WORKSPACE_ID": "wrong-stale-workspace"}

    def invoke_native(self, args, **env):
        return self.invoke([str(self.native)], args, **{**self.env, **env})

    def confirm(self, args, answer=b"yes\n", change=None):
        master, slave = pty.openpty()
        child = subprocess.Popen([str(self.native), *args, "--json"], cwd=self.fixture.repo,
                                 env={**self.fixture.env, **self.env}, stdin=slave, stderr=slave,
                                 stdout=subprocess.PIPE, text=True)
        os.close(slave)
        try:
            prompt = bytearray()
            deadline = time.monotonic() + 5
            while b"Close in Herdr instead? [y/N]" not in prompt:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0, prompt)
                self.assertTrue(select.select([master], [], [], remaining)[0], prompt)
                prompt.extend(os.read(master, 4096))
            self.assertIn(b"files and Git branches are kept", prompt)
            self.assertIn(b"Processes in its panes will be terminated", prompt)
            if change:
                change()
            if answer is None:
                child.send_signal(signal.SIGINT)
            else:
                os.write(master, answer)
            stdout, _ = child.communicate(timeout=5)
            return child.returncode, json.loads(stdout), bytes(prompt)
        finally:
            os.close(master)
            if child.poll() is None:
                child.kill()
                child.wait()

    def dispatched_target(self):
        call = next(c for c in reversed(self.server.calls) if c["method"] == "plugin.action.invoke")
        params = call["params"]
        self.assertEqual((params["plugin_id"], params["action_id"]), ("local.graftr", "close"))
        return params["context"], json.loads(params["context"]["correlation_id"])

    def worker(self, context):
        directory = self.fixture.root / "bin"
        return self.invoke([str(self.native)], ["close", "--json"], cwd=directory,
                           **{**self.env, "HERDR_WORKSPACE_ID": "parent", "HERDR_PLUGIN_ACTION_ID": "close",
                              "HERDR_PLUGIN_ROOT": str(directory), "HERDR_PLUGIN_CONTEXT_JSON": json.dumps(context)})

    def test_noninteractive_close_and_remove_require_confirmation_even_with_yes(self):
        for args in [["close"], ["remove"], ["remove", "--yes"], ["close", "@"]]:
            with self.subTest(args=args):
                result = self.invoke_native([*args, "--json"])
                self.assertEqual(result[0], 1, result)
                self.assertEqual(result[1]["error"]["code"], "CONFIRMATION_REQUIRED")
                self.assertEqual(result[1]["error"]["phase"], "confirmation")
                self.assertEqual(result[1]["progress"], [])
                self.assertFalse(self.fixture.state.exists())
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_acceptance_queues_close_only_without_configuration_or_deleting_dirty_files(self):
        (self.fixture.root / "config/graftr/projects.json").unlink()
        dirty = self.fixture.repo / "tracked.txt"
        dirty.write_text("keep these edits\n")
        untracked = self.fixture.repo / "new-file.txt"
        untracked.write_text("keep this too\n")
        for args in [["close"], ["remove", "@", "--yes"]]:
            with self.subTest(args=args):
                result = self.confirm(args)
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1]["outcome"], "queued")
                data = result[1]["data"]
                self.assertEqual((data["action"], data["managed"], data["status"]), ("close-workspace", False, "closing"))
                self.assertEqual(data["workspaceId"], "parent")
                self.assertEqual(data["logId"], "fixture-worker")
                _, target = self.dispatched_target()
                self.assertEqual(target["path"], str(self.fixture.repo))
                self.assertFalse(self.fixture.state.exists())
                self.assertTrue(self.server.associated)
                self.assertEqual(dirty.read_text(), "keep these edits\n")
                self.assertEqual(untracked.read_text(), "keep this too\n")
                self.assertEqual(self.fixture.git("branch", "--show-current").strip(), "main")
        self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))

    def test_decline_empty_eof_and_interrupt_do_not_dispatch(self):
        for answer in [b"n\n", b"\n", b"\x04", None]:
            with self.subTest(answer=answer):
                result = self.confirm(["remove"], answer=answer)
                self.assertEqual(result[0], 0, result)
                self.assertEqual(result[1]["outcome"], "cancelled")
                self.assertFalse(self.fixture.state.exists())
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_worker_closes_only_confirmed_workspace_and_preserves_checkout(self):
        self.confirm(["remove"])
        context, _ = self.dispatched_target()
        result = self.worker(context)
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["outcome"], "completed")
        self.assertEqual(result[1]["data"]["status"], "closed")
        self.assertFalse(self.server.associated)
        self.assertTrue(self.fixture.repo.is_dir())
        self.assertTrue((self.fixture.repo / ".env").is_file())
        self.assertEqual(self.fixture.git("rev-parse", "HEAD").strip(), self.fixture.commit)
        self.assertFalse(list(self.fixture.state.glob("*.json")))
        self.assertFalse((self.fixture.root / "home/forbidden-service").exists())
        self.assertFalse(self.server.errors)

    def test_workspace_rebinding_after_prompt_requires_new_confirmation(self):
        result = self.confirm(["close"], change=lambda: setattr(self.server, "parent_path", str(self.fixture.checkout)))
        self.assertEqual(result[0], 1, result)
        self.assertIn("identity changed", result[1]["error"]["message"])
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_caller_switch_after_prompt_does_not_follow_another_workspace(self):
        result = self.confirm(["close"], change=lambda: setattr(self.server, "caller", "owned"))
        self.assertEqual(result[0], 1, result)
        self.assertIn("caller workspace changed", result[1]["error"]["message"])
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_worker_rejects_changed_target_or_selected_workspace(self):
        self.confirm(["close"])
        context, _ = self.dispatched_target()
        self.server.parent_label = "Changed since confirmation"
        result = self.worker(context)
        self.assertEqual(result[0], 1, result)
        self.assertIn("identity changed", result[1]["error"]["message"])
        context["workspace_id"] = "owned"
        result = self.worker(context)
        self.assertEqual(result[0], 1, result)
        self.assertIn("selected Herdr action", result[1]["error"]["message"])
        self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))

    def test_worker_rejects_new_managed_ownership(self):
        self.confirm(["close"])
        context, _ = self.dispatched_target()
        created = self.invoke_native(["create", "bench", "sample", "--no-herdr", "--json"])
        self.assertEqual(created[0], 0, created)
        path = self.fixture.state / "bench--sample.json"
        record = json.loads(path.read_text())
        record.update(socket=self.server.path, workspaceId="parent")
        write_record(path, record)
        before = path.read_bytes()
        result = self.worker(context)
        self.assertEqual(result[0], 1, result)
        self.assertIn("task owns", result[1]["error"]["message"])
        self.assertEqual(path.read_bytes(), before)
        self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))

    def test_missing_or_explicit_target_and_project_filters_do_not_fall_back(self):
        for args in [["close", "missing"], ["remove", str(self.fixture.repo), "--yes"],
                     ["close", "--project", "bench"], ["cleanup"], ["nuke", "missing"]]:
            with self.subTest(args=args):
                result = self.invoke_native([*args, "--json"])
                self.assertEqual(result[0], 1, result)
                self.assertNotEqual(result[1]["error"]["code"], "CONFIRMATION_REQUIRED")
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_missing_workspace_does_not_prompt_and_failed_dispatch_does_not_close_directly(self):
        self.server.associated = False
        result = self.invoke_native(["close", "--json"])
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["error"]["phase"], "preflight")
        self.server.associated = True
        self.server.reject_dispatch = True
        result = self.confirm(["close"])
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))

    def test_plain_non_git_workspace_can_be_closed_without_inventing_checkout_ownership(self):
        self.server.parent_path = None
        result = self.confirm(["close"])
        self.assertEqual(result[0], 0, result)
        context, target = self.dispatched_target()
        self.assertIsNone(target["path"])
        self.assertEqual(self.worker(context)[0], 0)

    def test_unconfirmed_plugin_close_or_remove_cannot_approve_the_alternative(self):
        directory = self.fixture.root / "bin"
        for action in ["close", "remove"]:
            result = self.invoke([str(self.native)], [action, "--json"], cwd=directory,
                                 **{**self.env, "HERDR_WORKSPACE_ID": "parent", "HERDR_PLUGIN_ACTION_ID": action,
                                    "HERDR_PLUGIN_ROOT": str(directory),
                                    "HERDR_PLUGIN_CONTEXT_JSON": json.dumps({"workspace_id": "parent"})})
            self.assertEqual(result[0], 1, result)
            self.assertEqual(result[1]["error"]["code"], "CONFIRMATION_REQUIRED")
        self.assertFalse(any(c["method"] in ["plugin.action.invoke", "workspace.close"] for c in self.server.calls))

    def test_new_adoption_or_invalid_record_blocks_the_confirmed_worker(self):
        self.confirm(["close"])
        context, _ = self.dispatched_target()
        record = {"schemaVersion": 1, "id": "bench--external", "project": "bench",
                  "repo": str(self.fixture.repo), "path": str(self.fixture.checkout), "branch": "task/sample",
                  "adoptedAt": "2026-01-01T00:00:00.123Z", "socket": self.server.path,
                  "workspaceId": "parent", "localPort": None, "url": None,
                  "environment": "external", "dependencies": "external", "hooks": []}
        self.fixture.state.mkdir(parents=True, mode=0o700)
        path = write_record(self.fixture.state / "adopted/bench--external.json", record)
        result = self.worker(context)
        self.assertEqual(result[0], 1, result)
        self.assertIn("adopted external", result[1]["error"]["message"])
        path.unlink()
        write_record(self.fixture.state / "bench--broken.json", {"schemaVersion": 4})
        result = self.worker(context)
        self.assertEqual(result[0], 1, result)
        self.assertFalse(any(c["method"] == "workspace.close" for c in self.server.calls))
