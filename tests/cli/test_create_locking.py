import fcntl
import json
from pathlib import Path
import shutil
import subprocess
import sys

from test_cli import NativeCase
from status_fixture import write_record
import bench


class CreationLockingTests(NativeCase):
    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)

    def git_probe(self, publish=None):
        marker = self.fixture.root / "git-lock-probe.json"
        real_git = shutil.which("git")
        self.assertIsNotNone(real_git)
        shim = self.fixture.root / "bin/git"
        shim.write_text(f'''#!{sys.executable}
import fcntl, json, os, sys
from pathlib import Path
args = sys.argv[1:]
if "worktree" in args and args[args.index("worktree") + 1] == "add":
    state = Path({str(self.fixture.state)!r})
    state.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(state / "operation.lock", os.O_RDWR | os.O_CREAT, 0o600)
    with os.fdopen(fd, "r+") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            locked = False
        except BlockingIOError:
            locked = True
        intent = list((state / "creating").glob("*.json"))
        Path({str(marker)!r}).write_text(json.dumps({{"locked": locked, "intents": len(intent)}}))
        publish = {str(publish) if publish else None!r}
        if not locked and publish:
            record = json.loads(Path(publish).read_text())
            directory = state / "frozen"
            directory.mkdir(mode=0o700, exist_ok=True)
            destination = directory / (record["id"] + ".json")
            destination.write_text(json.dumps(record))
            destination.chmod(0o600)
os.execv({real_git!r}, [{real_git!r}, *args])
''')
        shim.chmod(0o700)
        return marker

    def frozen(self):
        for args in [["create", "bench", "sample", "--no-herdr", "--json"],
                     ["freeze", "bench--sample", "--yes", "--json"]]:
            result = self.invoke([str(self.native)], args)
            self.assertEqual(result[0], 0, result)
        path = self.fixture.state / "frozen/bench--sample.json"
        record = json.loads(path.read_text())
        path.unlink()
        return path, record

    def test_plain_and_managed_creation_hold_the_same_registry_lock_through_git_mutation(self):
        for flags in [[], ["--no-setup"]]:
            with self.subTest(flags=flags):
                self.fresh()
                marker = self.git_probe()
                result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", *flags, "--json"])
                self.assertEqual(result[0], 0, result)
                self.assertEqual(json.loads(marker.read_text()), {"locked": True, "intents": 1})
                self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
                self.assertEqual(result[1]["data"]["status"], "skipped" if flags else "ready")
                self.assertEqual((self.fixture.checkout / ".env").exists(), not bool(flags))
                self.assertEqual((self.fixture.state / "bench--sample.json").exists(), not bool(flags))

    def test_plain_creation_cannot_race_a_lock_participating_frozen_reservation_writer(self):
        path, record = self.frozen()
        candidate = self.fixture.root / "frozen-to-publish.json"
        write_record(candidate, record)
        marker = self.git_probe(candidate)
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--branch", "task/sample", "--no-setup", "--no-herdr", "--json"])
        self.assertEqual(result[0], 0, result)
        self.assertFalse(path.exists() and self.fixture.checkout.exists(),
                         "Creation raced a cooperating writer and checked out a frozen task's reserved branch/path")
        self.assertEqual(json.loads(marker.read_text())["locked"], True)
        self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)

    def test_plain_creation_waits_for_registry_lock_and_rechecks_new_frozen_reservation(self):
        path, record = self.frozen()
        child = None
        try:
            with (self.fixture.state / "operation.lock").open("r+") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                child = subprocess.Popen([str(self.native), "create", "bench", "sample", "--branch", "task/sample",
                                          "--no-setup", "--no-herdr", "--json"], cwd=self.fixture.repo,
                                         env=self.fixture.env, stdin=subprocess.DEVNULL,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
                with self.assertRaises(subprocess.TimeoutExpired):
                    child.communicate(timeout=.2)
                self.assertFalse(self.fixture.checkout.exists())
                self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))
                write_record(path, record)
            stdout, stderr = child.communicate(timeout=10)
            report = json.loads(stdout)
            self.assertEqual((child.returncode, stderr), (1, ""), report)
            self.assertIn("Task is frozen", report["error"]["message"])
            self.assertFalse(self.fixture.checkout.exists())
            self.assertEqual(json.loads(path.read_text()), record)
            self.assertFalse(list((self.fixture.state / "setup").glob("*.json")))
        finally:
            if child is not None:
                if child.poll() is None:
                    child.kill()
                child.communicate(timeout=10)

    def test_plain_creation_refuses_unsafe_registry_lock_without_creating_a_checkout(self):
        self.fixture.state.mkdir(parents=True, mode=0o700)
        lock = self.fixture.state / "operation.lock"
        lock.write_text("foreign lock contents")
        lock.chmod(0o644)
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-setup", "--no-herdr", "--json"])
        self.assertEqual(result[0], 1, result)
        self.assertIn("private regular file", result[1]["error"]["message"])
        self.assertFalse(self.fixture.checkout.exists())
        self.assertEqual(lock.read_text(), "foreign lock contents")
        self.assertNotIn("task/sample", self.fixture.git("branch", "--list"))
