import copy
import errno
import json
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import time

from test_cli import NativeCase
from status_fixture import write_record
import bench


class LifecycleTests(NativeCase):
    def managed(self, command=None):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"]) if command is None else self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--no-herdr", "--json"])[0], 0)
        (self.fixture.state / ".auto-prune").touch(mode=0o600)
        return self.fixture.checkout

    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def normalize(self, result):
        value = bench.normalize(copy.deepcopy(list(result)), self.fixture.root)
        def visits(value):
            if isinstance(value, dict):
                if "phase" in value and "name" in value and "elapsedMs" in value:
                    value["elapsedMs"] = "<duration>"
                for item in value.values():
                    visits(item)
            elif isinstance(value, list):
                for item in value:
                    visits(item)
        visits(value)
        return value

    def profile(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def test_cleanup_and_close_keep_checkout_branch_and_leases(self):
        path = self.managed()
        for command in ["cleanup", "close"]:
            for args in [[command, "bench--sample", "--json"], [command, "sample"]]:
                expected = self.invoke(self.reference, args)
                actual = self.invoke([str(self.native)], args)
                self.assertEqual(self.normalize(actual), self.normalize(expected))
            self.assertTrue(path.is_dir())
            self.assertTrue((self.fixture.state / "bench--sample.json").exists())
            self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)

    def test_remove_deletes_only_clean_checkout_and_releases_record_after_retirement(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            path = self.managed(command)
            result = self.invoke(command, ["remove", "bench--sample", "--yes", "--json"])
            self.assertEqual(result[0], 0)
            results.append(self.normalize(result))
            self.assertFalse(path.exists())
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            self.assertFalse(list((self.fixture.state / "setup").glob("*.json")))
            self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)
            self.assertEqual(self.fixture.git("worktree", "list", "--porcelain").count("worktree "), 1)
        self.assertEqual(results[0], results[1])

    def test_dirty_untracked_locked_hidden_index_and_confirmation_refuse_without_effects(self):
        path = self.managed()
        tracked = path / "tracked.txt"
        tracked.write_text("unsaved work\n")
        self.assertEqual(self.compare(["remove", "bench--sample", "--yes", "--json"])[1]["error"]["phase"], "preflight")
        tracked.write_text("committed\n")
        untracked = path / "keep.txt"
        untracked.write_text("untracked")
        self.compare(["remove", "bench--sample", "--yes", "--json"])
        untracked.unlink()
        self.fixture.git("worktree", "lock", str(path))
        self.compare(["remove", "bench--sample", "--yes", "--json"])
        self.fixture.git("worktree", "unlock", str(path))
        self.fixture.git("-C", str(path), "update-index", "--assume-unchanged", "tracked.txt")
        self.compare(["remove", "bench--sample", "--yes", "--json"])
        self.fixture.git("-C", str(path), "update-index", "--no-assume-unchanged", "tracked.txt")
        result = self.compare(["remove", "bench--sample", "--json"])
        self.assertEqual(result[1]["error"]["code"], "CONFIRMATION_REQUIRED")
        self.assertEqual(result[1]["error"]["phase"], "confirmation")
        self.assertTrue(path.is_dir())

    def test_current_attached_branch_is_retained_without_rebinding_historical_record(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            path = self.managed(command)
            self.fixture.git("-C", str(path), "checkout", "-b", "feature/current")
            result = self.invoke(command, ["remove", "bench--sample", "--yes", "--json"])
            results.append(self.normalize(result))
            self.assertEqual(result[0], 0)
            self.assertEqual(result[1]["data"]["branch"], "feature/current")
            self.assertEqual(result[1]["data"]["recordedBranch"], "task/sample")
            for branch in ["feature/current", "task/sample"]:
                self.assertEqual(self.fixture.git("rev-parse", branch).strip(), self.fixture.commit)
        self.assertEqual(results[0], results[1])

    def test_retire_failure_keeps_ownership_then_explicit_retry_skips_completed_cleanup(self):
        outcomes = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            cleanup_count = self.fixture.root / "tmp/cleanup-count"
            gate = self.fixture.root / "tmp/allow-retire"
            self.profile(hooks={"cleanup": [{"name": "cleanup", "command": ["sh", "-c", f'printf cleanup >> "{cleanup_count}"']}],
                                "retire": [{"name": "retire", "command": ["sh", "-c", f'test ! -d "$GRAFTR_WORKTREE_PATH" && test "$PWD" != "$GRAFTR_WORKTREE_PATH" && test -f "{gate}" || {{ printf synthetic-private-error >&2; exit 7; }}']}]})
            path = self.managed(command)
            first = self.invoke(command, ["remove", "bench--sample", "--yes", "--json"])
            self.assertEqual(first[0], 1)
            self.assertEqual(first[1]["outcome"], "partial")
            self.assertNotIn("synthetic-private-error", str(first))
            self.assertFalse(path.exists())
            record = json.loads((self.fixture.state / "bench--sample.json").read_text())
            self.assertEqual(record["cleanup"]["status"], "complete")
            self.assertEqual(record["retire"]["status"], "failed")
            self.assertEqual(record["status"], "closed")
            gate.touch()
            second = self.invoke(command, ["remove", "bench--sample", "--yes", "--json"])
            self.assertEqual(second[0], 0)
            self.assertEqual(cleanup_count.read_text(), "cleanup")
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            outcomes.append((self.normalize(first), self.normalize(second)))
        self.assertEqual(outcomes[0], outcomes[1])

    def test_cleanup_retry_keeps_successful_results_and_uses_recorded_configuration(self):
        outcomes = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            first_effect = self.fixture.root / "tmp/first-effect"
            gate = self.fixture.root / "tmp/cleanup-gate"
            self.profile(hooks={"cleanup": [{"name": "first", "command": ["sh", "-c", f'printf first >> "{first_effect}"']},
                                            {"name": "second", "command": ["sh", "-c", f'test -f "{gate}"']}]})
            self.managed(command)
            first = self.invoke(command, ["cleanup", "bench--sample", "--json"])
            self.assertEqual(first[0], 1)
            gate.touch()
            self.profile(hooks={})
            second = self.invoke(command, ["cleanup", "bench--sample", "--json"])
            self.assertEqual(second[0], 0)
            self.assertEqual(first_effect.read_text(), "first")
            record = json.loads((self.fixture.state / "bench--sample.json").read_text())
            self.assertEqual(record["cleanup"]["attempt"], 2)
            self.assertEqual(record["cleanup"]["status"], "complete")
            outcomes.append((self.normalize(first), self.normalize(second)))
        self.assertEqual(outcomes[0], outcomes[1])

    def test_interactive_cancellation_is_explicit_and_leaves_checkout_intact(self):
        path = self.managed()
        for command in [self.reference, [str(self.native)]]:
            master, slave = pty.openpty()
            child = subprocess.Popen([*command, "remove", "bench--sample", "--json"], cwd=self.fixture.repo,
                                     env=self.fixture.env, stdin=slave, stderr=slave, stdout=subprocess.PIPE, text=True)
            os.close(slave)
            try:
                prompt = bytearray()
                deadline = time.monotonic() + 5
                while b"Continue? [y/N]" not in prompt:
                    remaining = deadline - time.monotonic()
                    self.assertGreater(remaining, 0)
                    self.assertTrue(select.select([master], [], [], remaining)[0])
                    prompt.extend(os.read(master, 4096))
                self.assertIn(b"including ignored .env", prompt)
                os.write(master, b"n\n")
                stdout, _ = child.communicate(timeout=5)
                self.assertEqual(child.returncode, 0)
                self.assertEqual(json.loads(stdout)["outcome"], "cancelled")
                self.assertTrue(path.is_dir())
            finally:
                os.close(master)
                if child.poll() is None:
                    child.kill()
                    child.wait()
