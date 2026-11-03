import copy
import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from status_fixture import write_record, receipt_path, STAMP


class DoctorTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        path = self.fixture.state / "bench--sample.json"
        task = json.loads(path.read_text())
        receipt = next((self.fixture.state / "setup").glob("*.json"))
        return task, path, receipt

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.fixture.repo), *args], env=self.fixture.env,
                       capture_output=True, check=True, timeout=10)

    def test_completed_local_and_standalone_live_reports_and_formatting(self):
        task, _, _ = self.managed()
        for args in [["doctor", task["id"], "--json"], ["doctor", "sample", "--project", "bench", "--json"],
                     ["doctor", task["id"], "--live", "--json"]]:
            result = self.compare(args)
            self.assertEqual(result[0], 0)
            self.assertEqual(result[1]["data"]["status"], "ok")
        for width in [24, 40, 80, 120]:
            self.compare(["doctor", task["id"]], COLUMNS=str(width))
        self.compare(["doctor", "--json"], cwd=Path(task["path"]))
        self.compare(["doctor", "--json"], cwd=self.fixture.root)
        for color, term in [(True, "xterm"), (False, "xterm"), (True, "dumb")]:
            self.assertEqual(self.invoke_tty([str(self.native)], ["doctor", task["id"]], color, term),
                             self.invoke_tty(self.reference, ["doctor", task["id"]], color, term))

    def test_checkout_missing_locked_detached_and_wrong_registration(self):
        task, record, _ = self.managed()
        self.git("worktree", "lock", task["path"])
        self.assertEqual(self.compare(["doctor", task["id"], "--json"])[1]["data"]["findings"][0]["code"], "registration-needs-review")
        self.git("worktree", "unlock", task["path"])
        subprocess.run(["git", "-C", task["path"], "checkout", "--detach"], env=self.fixture.env, capture_output=True, check=True)
        self.compare(["doctor", task["id"], "--json"])
        shutil.rmtree(task["path"])
        self.compare(["doctor", task["id"], "--json"])
        self.git("worktree", "prune")
        self.compare(["doctor", task["id"], "--json"])
        Path(task["path"]).mkdir()
        self.compare(["doctor", task["id"], "--json"])
        task["path"] = str(self.fixture.repo)
        write_record(record, task)
        self.compare(["doctor", task["id"], "--json"])

    def test_receipt_absence_corruption_pending_and_inconsistent_completion(self):
        task, record, receipt_file = self.managed()
        original = json.loads(receipt_file.read_text())
        for changes in [{"status": "pending", "step": "dependencies", "dependencies": "pending"},
                        {"status": "incomplete", "step": "dependencies", "dependencies": "failed"},
                        {"status": "ready", "step": "route"}, {"workspaceId": "different-workspace", "socket": "/synthetic/server"}]:
            write_record(receipt_file, {**original, **changes})
            self.assertEqual(self.compare(["doctor", task["id"], "--json"])[0], 1)
        write_record(receipt_file, original)
        task["status"] = "preparing"
        write_record(record, task)
        self.compare(["doctor", task["id"], "--json"])
        receipt_file.write_text('{"private":"not-for-output"')
        result = self.compare(["doctor", task["id"], "--json"])
        self.assertNotIn("not-for-output", str(result))
        receipt_file.unlink()
        result = self.compare(["doctor", task["id"], "--json"])
        self.assertEqual(result[1]["error"]["code"], "DOCTOR_INCOMPLETE")

    def test_invalid_profile_is_unknown_without_reading_env_or_running_hooks(self):
        task, _, _ = self.managed()
        config_file = self.fixture.root / "config/graftr/projects.json"
        original = json.loads(config_file.read_text())
        bad = [{"wat": True}, {"localPorts": [0, 1]}, {"ports": {"bad_name": [1, 2]}},
               {"branchName": "{{secret}}"}, {"worktreePath": "../escape"}, {"workspaceLabel": "{{secret}}"},
               {"envOverrides": {"LD_PRELOAD": "secret"}}, {"autoSetup": "true"},
               {"installCommand": ["sh", "\n"]}, {"installInputs": ["../secret"]},
               {"healthChecks": [{"name": "probe", "type": "http", "path": "//example.test"}]},
               {"hooks": {"cleanup": [{"name": "bad", "command": ["./secret"]}]}}, {"setupHooks": []}]
        for changes in bad:
            config = copy.deepcopy(original)
            config["projects"]["bench"].update(changes)
            config_file.write_text(json.dumps(config))
            result = self.compare(["doctor", task["id"], "--json"])
            self.assertEqual(result[1]["data"]["findings"][1]["code"], "ownership-unavailable", changes)
        original["projects"]["bench"].update(envSource="/does-not-exist/private-env",
            hooks={"afterEnvironment": [{"name": "sentinel", "command": ["touch", str(self.fixture.root / "PWN")]}]})
        config_file.write_text(json.dumps(original))
        self.assertEqual(self.compare(["doctor", task["id"], "--json"])[0], 0)
        self.assertFalse((self.fixture.root / "PWN").exists())

    def test_conflicting_ownership_and_profile_repository(self):
        task, _, _ = self.managed()
        duplicate = {**task, "id": "bench--duplicate"}
        other = write_record(self.fixture.state / "bench--duplicate.json", duplicate)
        self.compare(["doctor", task["id"], "--json"])
        other.unlink()
        config_file = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(config_file.read_text())
        config["projects"]["bench"]["repo"] = str(self.fixture.root)
        config_file.write_text(json.dumps(config))
        self.compare(["doctor", task["id"], "--json"])

    def test_live_workspace_identity_outage_and_closed_state(self):
        task, record, receipt_file = self.managed()
        receipt = json.loads(receipt_file.read_text())
        owned = {"workspace_id": "owned", "label": "Owned", "worktree": {"checkout_path": task["path"]}}
        for status, spaces, code in [("ready", [owned], "workspace-matches"), ("ready", [], "workspace-missing"),
                ("ready", [owned, owned], "workspace-conflict"),
                ("ready", [{**owned, "workspace_id": "other"}], "workspace-conflict"),
                ("closed", [], "workspace-closed"), ("closed", [owned], "closed-workspace-present"),
                ("ready", [{"workspace_id": "private-invalid"}], "workspace-unavailable"),
                ("ready", None, "workspace-unavailable")]:
            socket_path, calls, errors = self.serve(lambda request: None if spaces is None else {"id": request["id"], "result": {"workspaces": spaces}})
            task.update(socket=socket_path, workspaceId="owned", status=status)
            write_record(record, task)
            receipt.update(socket=socket_path, workspaceId="owned")
            receipt_file.unlink()
            receipt_file = write_record(receipt_path(self.fixture.state, receipt), receipt)
            result = self.compare(["doctor", task["id"], "--live", "--json"], HERDR_SOCKET_PATH="/wrong/focused.sock")
            self.assertEqual(result[1]["data"]["findings"][3]["code"], code)
            self.assertEqual([call["method"] for call in calls], ["workspace.list"] * 2)
            self.assertFalse(errors)

    def test_adopted_external_setup_remains_incomplete_without_replaying_stages(self):
        path = self.fixture.root / "external"
        self.git("worktree", "add", "-b", "external", str(path))
        record = {"schemaVersion": 1, "id": "bench--external", "project": "bench", "repo": str(self.fixture.repo),
                  "path": str(path), "branch": "external", "adoptedAt": STAMP, "socket": None, "workspaceId": None,
                  "localPort": None, "url": None, "environment": "external", "dependencies": "external", "hooks": []}
        record_file = write_record(self.fixture.state / "adopted/bench--external.json", record)
        for args in [["doctor", record["id"], "--json"], ["doctor", "--json"], ["doctor", record["id"], "--live", "--json"], ["doctor", record["id"]]]:
            self.assertEqual(self.compare(args, cwd=path)[0], 1)
        record["dependencies"] = "pending"
        write_record(record_file, record)
        self.compare(["doctor", record["id"], "--json"])
        self.git("worktree", "lock", str(path))
        self.compare(["doctor", record["id"], "--json"])
