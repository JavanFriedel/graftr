import fcntl
import json

from test_cli import NativeCase
from status_fixture import write_record, setup, receipt_path
import bench


class ProvisionTests(NativeCase):
    def run_cli(self, *args, cwd=None, **env):
        result = self.invoke([str(self.native)], [*args, "--json"], cwd=cwd, **env)
        self.assertFalse((self.fixture.root / "home/forbidden-service").exists())
        return result

    def profile(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def external(self):
        path = self.fixture.root / "external"
        self.fixture.git("worktree", "add", "-qb", "feature/external", str(path))
        (self.fixture.repo / ".env").chmod(0o600)
        result = self.run_cli("adopt", str(path))
        self.assertEqual(result[0], 0, result)
        record_path = self.fixture.state / "adopted/bench--external.json"
        return path, record_path

    def assert_not_converted(self, path, record_path):
        self.assertTrue(record_path.is_file())
        self.assertFalse((self.fixture.state / "bench--external.json").exists())
        self.assertFalse((self.fixture.state / "migration-backups").exists())
        self.assertFalse((path / "node_modules").exists())

    def test_preview_is_readonly_and_does_not_disclose_baseline_or_reserve_ports(self):
        path, record_path = self.external()
        before = bench.readonly_snapshot(self.fixture)
        result = self.run_cli("adopt", str(path), "--provision", "--dry-run")
        self.assertEqual((result[0], result[1]["outcome"]), (0, "completed"), result)
        self.assertEqual(result[1]["data"]["status"], "preview")
        self.assertEqual(result[1]["data"]["localPort"], 40000)
        self.assertEqual(result[1]["progress"], [])
        self.assertNotIn("synthetic-initial", str(result))
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        self.assert_not_converted(path, record_path)
        self.assertFalse((path / ".env").exists())

    def test_human_output_and_profiles_without_dotenv_support_provisioning(self):
        path, _ = self.external()
        config_path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(config_path.read_text())
        del config["projects"]["bench"]["envSource"]
        config_path.write_text(json.dumps(config))
        result = self.invoke([str(self.native)], ["adopt", str(path), "--provision", "--dry-run"], COLUMNS="40")
        self.assertEqual(result[0], 0, result)
        self.assertIn("preview", result[1])
        result = self.invoke([str(self.native)], ["adopt", str(path), "--provision"])
        self.assertEqual(result[0], 0, result)
        self.assertIn("bench--external", result[1])
        self.assertFalse((path / ".env").exists())

    def test_completion_requires_provision_before_offering_dry_run(self):
        for words, expected in [(["adopt", "--"], False), (["adopt", "--provision", "--"], True)]:
            result = self.invoke([str(self.native)], ["_complete", "--", *words])
            self.assertEqual("--dry-run" in result[1].splitlines(), expected)
        result = self.invoke([str(self.native)], ["_complete", "--", "adopt", "--"])
        self.assertIn("--provision", result[1].splitlines())

    def test_provision_preserves_id_path_branch_dirty_work_and_uses_normal_setup(self):
        path, record_path = self.external()
        original = record_path.read_bytes()
        (path / "tracked.txt").write_text("keep my uncommitted work\n")
        (path / "untracked.txt").write_text("keep my new file\n")
        before = self.fixture.git("-C", str(path), "status", "--porcelain=v1", "-z")
        self.profile(hooks={
            "afterEnvironment": [{"name": "before", "command": ["sh", "-c", "test -f .env && printf before > node-order"]}],
            "afterDependencies": [{"name": "after", "command": ["sh", "-c", "test -f node_modules/fixture && printf after >> node-order"]}],
            "cleanup": [{"name": "cleanup", "command": ["true"]}],
        })
        result = self.run_cli("adopt", str(path), "--provision", HERDR_ENV="1", HERDR_SOCKET_PATH="/not-a-live-socket")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((result[1]["command"], result[1]["data"]["status"]), ("adopt", "ready"))
        self.assertNotIn("synthetic-initial", str(result))
        task = json.loads((self.fixture.state / "bench--external.json").read_text())
        self.assertEqual((task["id"], task["path"], task["branch"]), ("bench--external", str(path), "feature/external"))
        self.assertIsNone(task["socket"])
        self.assertEqual(task["cleanup"]["status"], "armed")
        self.assertEqual((path / "tracked.txt").read_text(), "keep my uncommitted work\n")
        self.assertEqual((path / "untracked.txt").read_text(), "keep my new file\n")
        self.assertEqual((path / "node-order").read_text(), "beforeafter")
        self.assertIn("PORT=40000", (path / ".env").read_text())
        self.assertEqual((path / ".env").stat().st_mode & 0o777, 0o600)
        self.assertFalse(record_path.exists())
        backups = list((self.fixture.state / "migration-backups").glob("adoption-*.json"))
        self.assertEqual(len(backups), 1)
        self.assertEqual(backups[0].read_bytes(), original)
        self.assertEqual(backups[0].stat().st_mode & 0o777, 0o600)
        self.assertIn(before.split("\0")[0], self.fixture.git("-C", str(path), "status", "--porcelain=v1", "-z"))
        self.assertEqual(self.run_cli("doctor", task["id"])[0], 0)
        # Conversion is never an implicit replay of the installer or hooks.
        result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1)
        self.assertEqual((path / "node_modules/install-count").read_text().strip(), "1")

    def test_installer_failure_retains_managed_ownership_and_recovers_via_explicit_setup(self):
        path, record_path = self.external()
        self.profile(installCommand=["sh", "-c", "printf private-install-error >&2; exit 7"])
        result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertEqual(result[1]["data"]["status"], "incomplete")
        self.assertFalse(record_path.exists())
        self.assertTrue((self.fixture.state / "bench--external.json").is_file())
        self.assertNotIn("private-install-error", str(result))
        self.profile(installCommand=["sh", "-c", "mkdir -p node_modules; printf installed > node_modules/fixture"])
        result = self.run_cli("setup", "--no-herdr", cwd=path)
        self.assertEqual(result[0], 0, result)

    def test_manual_addresses_attachments_and_selected_stages_are_not_claimed(self):
        path, record_path = self.external()
        original = json.loads(record_path.read_text())
        for changes in [{"localPort": 32123}, {"url": "https://external.example.test:5443"},
                        {"socket": "/external.sock", "workspaceId": "external"}, {"dependencies": "pending"},
                        {"dependencies": "failed"}, {"environment": "completed"},
                        {"hooks": [{"phase": "afterDependencies", "name": "manual", "status": "completed"}]}]:
            write_record(record_path, {**original, **changes})
            for flags in [["--dry-run"], []]:
                result = self.run_cli("adopt", str(path), "--provision", *flags)
                self.assertEqual(result[0], 1, (changes, result))
                self.assertEqual(result[1]["progress"], [])
                self.assert_not_converted(path, record_path)

    def test_existing_environment_and_unsafe_baseline_are_preserved(self):
        path, record_path = self.external()
        target = path / ".env"
        target.write_text("SECRET=keep-this\n")
        result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(target.read_text(), "SECRET=keep-this\n")
        self.assert_not_converted(path, record_path)
        target.unlink()
        target.symlink_to(self.fixture.repo / ".env")
        self.assertEqual(self.run_cli("adopt", str(path), "--provision")[0], 1)
        target.unlink()
        (self.fixture.repo / ".env").chmod(0o644)
        self.assertEqual(self.run_cli("adopt", str(path), "--provision")[0], 1)
        self.assert_not_converted(path, record_path)

    def test_identity_change_and_existing_setup_receipt_fail_closed(self):
        path, record_path = self.external()
        self.fixture.git("-C", str(path), "switch", "-qc", "feature/changed")
        result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1, result)
        self.assert_not_converted(path, record_path)
        self.fixture.git("-C", str(path), "switch", "-q", "feature/external")
        receipt = setup(None, path)
        write_record(receipt_path(self.fixture.state, receipt), receipt)
        result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1, result)
        self.assert_not_converted(path, record_path)

    def test_interrupted_transfer_with_both_records_remains_fail_closed(self):
        path, record_path = self.external()
        result = self.run_cli("create", "bench", "sample", "--no-herdr")
        self.assertEqual(result[0], 0, result)
        task = json.loads((self.fixture.state / "bench--sample.json").read_text())
        task.update(id="bench--external", path=str(path), branch="feature/external")
        write_record(self.fixture.state / "bench--external.json", task)
        original = record_path.read_bytes()
        for args in [["adopt", str(path), "--provision"], ["setup", "--no-herdr"],
                     ["open", "bench--external", "--no-herdr"]]:
            result = self.run_cli(*args, cwd=path)
            self.assertEqual(result[0], 1, result)
        self.assertEqual(record_path.read_bytes(), original)
        self.assertTrue((self.fixture.state / "bench--external.json").is_file())
        self.assertFalse((path / ".env").exists())

    def test_environment_appearing_during_address_discovery_is_not_overwritten(self):
        path, record_path = self.external()
        self.profile(httpsPorts=[5200, 5300], url="https://fixture.example.test:{{httpsPort}}",
                     hooks={"afterDependencies": [{"name": "publish", "command": ["graftr", "hook", "tailscale"]}]})
        stub = self.fixture.root / "bin/tailscale"
        stub.write_text('#!/bin/sh\nprintf "SECRET=keep-this\\n" > "$PROVISION_TARGET/.env"\nprintf "{}\\n"\n')
        stub.chmod(0o700)
        result = self.run_cli("adopt", str(path), "--provision", PROVISION_TARGET=str(path))
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["progress"], [])
        self.assertEqual((path / ".env").read_text(), "SECRET=keep-this\n")
        self.assert_not_converted(path, record_path)

    def test_active_checkout_lock_refuses_conversion_without_waiting_or_mutation(self):
        path, record_path = self.external()
        lock = next(self.fixture.state.glob("setup-*.lock"))
        with lock.open("w") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_cli("adopt", str(path), "--provision")
        self.assertEqual(result[0], 1, result)
        self.assert_not_converted(path, record_path)

    def test_invalid_combinations_require_explicit_provision_intent(self):
        for args in [["status", "--provision"], ["adopt", "--dry-run"],
                     ["adopt", "--provision", "--install"], ["adopt", "--provision", "--environment"],
                     ["adopt", "--provision", "--hook", "afterDependencies/manual"],
                     ["adopt", "--provision", "--apply"], ["adopt", "--provision=true"]]:
            result = self.run_cli(*args)
            self.assertEqual(result[0], 1, result)
            self.assertEqual(result[1]["error"]["code"], "INVALID_ARGUMENTS")
        self.assertFalse(self.fixture.state.exists())
