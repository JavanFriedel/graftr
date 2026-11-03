import copy
import json
from pathlib import Path
import shutil
import subprocess
import time
import fcntl

from test_cli import NativeCase
from status_fixture import write_record, plan
import bench


class SetupTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        return self.fixture.checkout

    def normalized(self, result):
        return bench.normalize(list(result), self.fixture.root)

    def setup_pair(self, path, *args, **env):
        expected = self.invoke(self.reference, ["setup", "--no-herdr", *args, "--json"], cwd=path, **env)
        actual = self.invoke([str(self.native)], ["setup", "--no-herdr", *args, "--json"], cwd=path, **env)
        self.assertEqual(self.normalized(actual), self.normalized(expected))
        return actual

    def profile(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def test_setup_reuses_source_dependency_fingerprint_and_native_receipts_are_source_readable(self):
        path = self.managed()
        receipt_file = next((self.fixture.state / "setup").glob("*.json"))
        key = json.loads(receipt_file.read_text())["installKey"]
        result = self.setup_pair(path)
        self.assertEqual(result[0], 0)
        self.assertEqual(result[1]["data"]["timings"]["dependencies"], "reused")
        self.assertEqual(json.loads(receipt_file.read_text())["installKey"], key)
        self.assertEqual((path / "node_modules/install-count").read_text().strip(), "1")
        self.compare(["doctor", "bench--sample", "--json"])
        self.compare(["status", "--json"])

    def test_explicit_install_skip_install_and_no_bun_runtime(self):
        path = self.managed()
        self.setup_pair(path, "--install")
        self.assertEqual((path / "node_modules/install-count").read_text().strip(), "3")
        result = self.setup_pair(path, "--skip-install")
        self.assertEqual(result[1]["data"]["dependencies"], "skipped")
        result = self.setup_pair(path, PATH="/usr/bin:/bin")
        self.assertEqual(result[0], 0)

    def test_native_enrollment_of_linked_checkout_matches_source_without_adopting_main_checkout(self):
        path = self.fixture.root / "trees/unmanaged"
        subprocess.run(["git", "-C", str(self.fixture.repo), "worktree", "add", "-b", "feature/unmanaged", str(path)], env=self.fixture.env, capture_output=True, check=True)
        results = []
        for command in [self.reference, [str(self.native)]]:
            if self.fixture.state.exists():
                shutil.rmtree(self.fixture.state)
            (path / ".env").unlink(missing_ok=True)
            if (path / "node_modules").exists():
                shutil.rmtree(path / "node_modules")
            result = self.invoke(command, ["setup", "--no-herdr", "--json"], cwd=path)
            results.append(self.normalized(result))
            self.assertEqual(result[0], 0)
            self.assertEqual((path / "node_modules/install-count").read_text().strip(), "1")
            self.assertEqual((path / ".env").stat().st_mode & 0o777, 0o600)
        self.assertEqual(results[0], results[1])
        self.setup_pair(self.fixture.repo)

    def test_missing_required_outputs_invalidates_cache_without_rewriting_environment(self):
        path = self.managed()
        original = (path / ".env").read_bytes()
        (path / "node_modules/fixture").unlink()
        result = self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=path)
        self.assertEqual(result[0], 0)
        self.assertEqual(result[1]["data"]["timings"]["dependencies"], "installed")
        self.assertEqual((path / ".env").read_bytes(), original)
        self.setup_pair(path)

    def test_installer_failure_keeps_checkout_private_output_and_incomplete_receipt(self):
        path = self.managed()
        self.profile(installCommand=["sh", "-c", "printf synthetic-private-error >&2; exit 7"])
        result = self.setup_pair(path)
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["data"]["dependencies"], "failed")
        self.assertEqual(result[1]["error"]["code"], "SETUP_INCOMPLETE")
        self.assertNotIn("synthetic-private-error", str(result))
        self.assertTrue(path.is_dir())
        self.assertTrue(any("synthetic-private-error" in file.read_text() for file in (self.fixture.state / "logs/bench--sample").glob("*.log")))

    def test_hooks_use_current_attempt_context_and_record_bounded_failures(self):
        path = self.managed()
        self.profile(hooks={"afterEnvironment": [{"name": "before", "command": ["sh", "-c", 'test "$GRAFTR_TASK_ID" = bench--sample && test "$GRAFTR_DEPENDENCIES" = pending && printf before > hook-order']}],
                            "afterDependencies": [{"name": "after", "command": ["sh", "-c", 'test "$GRAFTR_DEPENDENCIES" = ready && printf after >> hook-order']}]})
        # Hook durations are volatile but the phase/name/status/exit/log binding is not.
        expected = self.invoke(self.reference, ["setup", "--no-herdr", "--json"], cwd=path)
        actual = self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=path)
        for result in [expected, actual]:
            for hook in result[1]["data"]["hooks"]:
                self.assertGreaterEqual(hook["elapsedMs"], 0)
                hook["elapsedMs"] = "<duration>"
        self.assertEqual(self.normalized(actual), self.normalized(expected))
        self.assertEqual((path / "hook-order").read_text(), "beforeafter")
        self.profile(hooks={"afterDependencies": [{"name": "timeout", "command": ["sh", "-c", "sleep 60"], "timeoutMs": 100}]})
        result = self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=path)
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["data"]["hooks"][0]["reason"], "timeout")
        self.assertEqual(result[1]["progress"][-2]["status"], "completed")

    def test_environment_drift_tracked_dotenv_and_cleanup_pending_preserve_ownership(self):
        path = self.managed()
        original = (path / ".env").read_bytes()
        (path / ".env").write_bytes(original + b"\nPORT=1234\n")
        result = self.setup_pair(path)
        self.assertEqual(result[0], 1)
        self.assertNotIn("1234", str(result))
        (path / ".env").write_bytes(original)
        self.setup_pair(path)
        record = self.fixture.state / "bench--sample.json"
        task = json.loads(record.read_text())
        task["cleanup"] = plan("cleanup", "failed")
        write_record(record, task)
        self.setup_pair(path)

    def test_automatic_history_expiry_runs_once_and_keeps_active_records(self):
        path = self.managed()
        directory = self.fixture.state / "removed"
        directory.mkdir(mode=0o700)
        old = directory / "old"
        old.write_text("expired synthetic record")
        import os
        os.utime(old, (1767225600, 1767225600))
        result = self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=path)
        self.assertEqual(result[0], 0)
        self.assertFalse(old.exists())
        marker = self.fixture.state / ".auto-prune"
        self.assertTrue(marker.is_file())
        stamp = marker.stat().st_mtime_ns
        self.invoke([str(self.native)], ["setup", "--no-herdr", "--json"], cwd=path)
        self.assertEqual(marker.stat().st_mtime_ns, stamp)
        self.assertTrue((self.fixture.state / "bench--sample.json").exists())

    def test_cancelled_or_removed_attempt_cannot_restore_readiness_after_installer_finishes(self):
        path = self.managed()
        record_path = self.fixture.state / "bench--sample.json"
        receipt_path = next((self.fixture.state / "setup").glob("*.json"))
        initial_task = json.loads(record_path.read_text())
        initial_receipt = json.loads(receipt_path.read_text())
        started = self.fixture.root / "tmp/installer-started"
        release = self.fixture.root / "tmp/installer-release"
        self.profile(installCommand=["sh", "-c", f'printf started > "{started}"; while [ ! -e "{release}" ]; do sleep 0.01; done; mkdir -p node_modules; printf installed > node_modules/fixture'])
        for remove in [False, True]:
            results = []
            for command in [self.reference, [str(self.native)]]:
                started.unlink(missing_ok=True)
                release.unlink(missing_ok=True)
                write_record(record_path, initial_task)
                write_record(receipt_path, initial_receipt)
                child = subprocess.Popen([*command, "setup", "--no-herdr", "--install", "--json"], cwd=path,
                                         env=self.fixture.env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                try:
                    deadline = time.monotonic() + 5
                    while not started.exists() and time.monotonic() < deadline:
                        if child.poll() is not None:
                            break
                        time.sleep(.005)
                    self.assertTrue(started.exists(), "installer never reached the controlled wait")
                    with (self.fixture.state / "operation.lock").open("w") as lock:
                        fcntl.flock(lock, fcntl.LOCK_EX)
                        if remove:
                            record_path.unlink()
                            receipt_path.unlink()
                        else:
                            receipt = json.loads(receipt_path.read_text())
                            receipt.update(status="incomplete", runId=None, message="Fixture cancellation; no resources may be restored.")
                            task = json.loads(record_path.read_text())
                            task["status"] = "closed"
                            write_record(receipt_path, receipt)
                            write_record(record_path, task)
                    release.touch()
                    stdout, stderr = child.communicate(timeout=10)
                    result = (child.returncode, json.loads(stdout), stderr)
                    results.append(self.normalized(result))
                    self.assertEqual(result[0], 1)
                    self.assertEqual(result[1]["data"]["status"], "incomplete")
                    if remove:
                        self.assertFalse(record_path.exists())
                        self.assertFalse(receipt_path.exists())
                    else:
                        self.assertEqual(json.loads(record_path.read_text())["status"], "closed")
                finally:
                    release.touch()
                    if child.poll() is None:
                        child.kill()
                        child.wait()
            self.assertEqual(results[0], results[1])
