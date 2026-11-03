import copy
import json
from pathlib import Path
import re
import subprocess

from test_cli import NativeCase
from status_fixture import write_record
import bench


class AdoptionTests(NativeCase):
    def external(self, dirname="external"):
        path = self.fixture.root / dirname
        subprocess.run(["git", "-C", str(self.fixture.repo), "worktree", "add", "-b", "feature/external", str(path)],
                       env=self.fixture.env, capture_output=True, check=True)
        (path / ".env").write_text("PORT=32123\nBASE_URL=https://external.example.test:5443\nSECRET=synthetic-local\n")
        (path / ".env").chmod(0o644)
        return path

    def profile(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def normalize(self, value):
        if isinstance(value, dict):
            return {key: "<clock>" if key == "adoptedAt" else self.normalize(item) for key, item in value.items()}
        if isinstance(value, (list, tuple)):
            return [self.normalize(item) for item in value]
        if isinstance(value, str):
            value = re.sub(re.escape(str(self.fixture.state)) + r"/logs/[^ /]+/[^ /]+\.log", "<private-log>", value)
            value = re.sub(re.escape(str(self.fixture.state)) + r"/env-backups/[^ /]+\.env", "<private-backup>", value)
        return value

    def pair(self, path, args, reset=None, **env):
        results = []
        for command in [self.reference, [str(self.native)]]:
            if reset:
                reset()
            directory = self.fixture.state / "adopted"
            if directory.exists():
                for file in directory.glob("*.json"):
                    file.unlink()
            result = self.invoke(command, ["adopt", str(path), *args, "--json"], **env)
            results.append(self.normalize(result))
        self.assertEqual(results[0], results[1])
        return results[1]

    def test_default_adoption_preserves_checkout_observes_public_address_and_is_idempotent(self):
        path = self.external()
        original = (path / ".env").read_bytes()
        before = {str(file.relative_to(path)): file.read_bytes() for file in path.rglob("*") if file.is_file()}
        result = self.pair(path, [])
        self.assertEqual(result[0], 0)
        record = result[1]["data"]["worktree"]
        self.assertEqual(record["id"], "bench--external")
        self.assertEqual((record["localPort"], record["url"]), (32123, "https://external.example.test:5443"))
        self.assertEqual((record["environment"], record["dependencies"]), ("external", "external"))
        self.assertNotIn("synthetic-local", str(result))
        self.assertEqual({str(file.relative_to(path)): file.read_bytes() for file in path.rglob("*") if file.is_file()}, before)
        self.assertEqual((path / ".env").read_bytes(), original)
        self.compare(["adopt", str(path), "--json"])
        for columns in [24, 40, 80]:
            self.compare(["adopt", str(path)], COLUMNS=str(columns))
        self.compare(["adopt", "--json"], cwd=path)
        record_file = next((self.fixture.state / "adopted").glob("*.json"))
        self.assertEqual(record_file.stat().st_mode & 0o777, 0o600)
        self.assertFalse(list(self.fixture.state.glob("*.json")))

    def test_missing_or_unsafe_environment_never_invents_addresses(self):
        path = self.external()
        target = path / ".env"
        target.chmod(0o666)
        result = self.pair(path, [])
        self.assertIsNone(result[1]["data"]["worktree"]["localPort"])
        self.assertIsNone(result[1]["data"]["worktree"]["url"])
        target.unlink()
        self.pair(path, [])
        target.write_text("bad syntax\nPORT=32123\nBASE_URL=https://external.example.test\nSECRET='unterminated\n")
        target.chmod(0o644)
        result = self.pair(path, [])
        self.assertEqual(result[1]["data"]["worktree"]["localPort"], 32123)

    def test_nonstandard_checkout_names_get_same_stable_hashed_identity(self):
        path = self.external("External workspace 日本語")
        self.pair(path, [])

    def test_identity_main_symlink_locked_and_detached_refusals(self):
        path = self.external()
        for selector in [self.fixture.repo, self.fixture.root / "not-present"]:
            # OS-specific missing-path text is not reproduced; both must refuse safely.
            for command in [self.reference, [str(self.native)]]:
                self.assertEqual(self.invoke(command, ["adopt", str(selector), "--json"])[0], 1)
        link = self.fixture.root / "link"
        link.symlink_to(path)
        self.compare(["adopt", str(link), "--json"])
        subprocess.run(["git", "-C", str(self.fixture.repo), "worktree", "lock", str(path)], env=self.fixture.env, check=True)
        self.compare(["adopt", str(path), "--json"])
        subprocess.run(["git", "-C", str(self.fixture.repo), "worktree", "unlock", str(path)], env=self.fixture.env, check=True)
        subprocess.run(["git", "-C", str(path), "checkout", "--detach"], env=self.fixture.env, capture_output=True, check=True)
        self.compare(["adopt", str(path), "--json"])

    def test_environment_preview_and_apply_protect_manual_public_assignments(self):
        path = self.external()
        target = path / ".env"
        target.chmod(0o600)
        baseline = self.fixture.repo / ".env"
        baseline.write_text("PORT=3000\nBASE_URL=https://baseline.example.test\nSECRET=synthetic-rotated\nADDED='synthetic-new'\n")
        baseline.chmod(0o600)
        self.profile(envSource=str(baseline))
        self.pair(path, ["--environment"])
        self.assertFalse((self.fixture.state / "adopted").exists())
        original = target.read_bytes()
        result = self.pair(path, ["--environment", "--apply", "--take-baseline", "SECRET"], reset=lambda: target.write_bytes(original))
        self.assertEqual(result[0], 0)
        self.assertEqual(result[1]["data"]["worktree"]["environment"], "completed")
        self.assertEqual(result[1]["progress"], [
            {"action": "save-adoption", "target": "bench--external", "status": "completed"},
            {"action": "save-adoption", "target": "bench--external", "status": "completed"},
            {"action": "backup-environment", "target": "bench--external", "status": "completed"},
            {"action": "refresh-environment", "target": "bench--external", "status": "completed"},
            {"action": "save-adoption", "target": "bench--external", "status": "completed"},
        ])
        self.assertTrue(target.read_bytes().startswith(original))
        for backup in (self.fixture.state / "env-backups").glob("*.env"):
            self.assertEqual(backup.read_bytes(), original)
            self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
        self.pair(path, ["--environment", "--take-baseline", "PORT"])
        self.pair(path, ["--environment", "--install"])
        self.pair(path, ["--apply"])

    def test_explicit_installer_and_hooks_are_private_ordered_and_do_not_take_over_manual_services(self):
        path = self.external()
        self.profile(installCommand=["sh", "-c", "printf installer-private; printf install >> order"], hooks={
            "afterEnvironment": [{"name": "before", "command": ["sh", "-c", 'test "$GRAFTR_TASK_ID" = bench--external && test "$GRAFTR_LOCAL_PORT" = 32123 && test -z "$GRAFTR_PORT_STALE" && test -z "$GRAFTR_CLEANUP_RUN_ID" && printf before >> order']}],
            "afterDependencies": [{"name": "after", "command": ["sh", "-c", 'test "$GRAFTR_DEPENDENCIES" = completed && printf after >> order']}],
        })
        def reset():
            (path / "order").unlink(missing_ok=True)
        result = self.pair(path, ["--install", "--hook", "after", "--hook=before"], reset=reset,
                           GRAFTR_PORT_STALE="other-task", GRAFTR_CLEANUP_RUN_ID="other-run")
        self.assertEqual(result[0], 0)
        self.assertEqual((path / "order").read_text(), "beforeinstallafter")
        self.assertNotIn("installer-private", str(result))
        self.assertEqual(result[1]["data"]["worktree"]["dependencies"], "completed")
        logs = list((self.fixture.state / "logs/bench--external").glob("*.log"))
        self.assertEqual(len(logs), 6)
        self.assertTrue(all(log.stat().st_mode & 0o777 == 0o600 for log in logs))
        self.assertTrue(any("installer-private" in log.read_text() for log in logs))
        self.profile(hooks={"afterEnvironment": [{"name": "unsafe", "command": ["graftr", "hook", "tailscale", "enable"]}]})
        result = self.pair(path, ["--hook", "unsafe"])
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["progress"], [])

    def test_selected_stage_failure_remains_recorded_and_no_default_replay_occurs(self):
        path = self.external()
        self.profile(installCommand=["sh", "-c", "printf synthetic-private-error >&2; exit 7"])
        result = self.pair(path, ["--install"])
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["outcome"], "partial")
        record = json.loads(next((self.fixture.state / "adopted").glob("*.json")).read_text())
        self.assertEqual(record["dependencies"], "failed")
        self.assertNotIn("synthetic-private-error", str(result))
        self.compare(["adopt", str(path), "--json"])
        self.profile(hooks={"afterDependencies": [{"name": "timeout", "command": ["sh", "-c", "sleep 60"], "timeoutMs": 100}]})
        result = self.pair(path, ["--hook", "timeout"])
        self.assertEqual(result[0], 1)
        record = json.loads(next((self.fixture.state / "adopted").glob("*.json")).read_text())
        self.assertEqual(record["hooks"][0]["status"], "failed")

    def test_optional_attachment_uses_exact_path_without_opening_or_modifying_workspaces(self):
        path = self.external()
        config_file = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(config_file.read_text())
        config["herdr"] = "auto"
        config_file.write_text(json.dumps(config))
        socket_path, calls, errors = self.serve(lambda request: {"id": request["id"], "result": {"workspaces": [{"workspace_id": "owned-external", "label": "External", "worktree": {"checkout_path": str(path)}}]}})
        result = self.pair(path, [], HERDR_ENV="1", HERDR_SOCKET_PATH=socket_path)
        self.assertEqual(result[1]["data"]["worktree"]["workspaceId"], "owned-external")
        self.assertEqual([call["method"] for call in calls], ["workspace.list"] * 2)
        self.assertFalse(errors)

    def test_port_conflicts_with_provisioned_records_are_refused(self):
        path = self.external()
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        record_file = self.fixture.state / "bench--sample.json"
        task = json.loads(record_file.read_text())
        task["localPort"] = 32123
        task["overrides"]["PORT"] = "32123"
        write_record(record_file, task)
        self.compare(["adopt", str(path), "--json"])
