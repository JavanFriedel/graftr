import json
import hashlib
import math
import os
import fcntl
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
from test_herdr_setup import HerdrFixture
import bench


class TransferTests(NativeCase):
    def setUp(self):
        super().setUp()
        self.other = bench.Fixture(self.fixture.root.parent / "other", [str(self.native)], 30)
        self.assertEqual(self.fixture.commit, self.other.commit)
        for fixture in (self.fixture, self.other):
            (fixture.root / "trees").mkdir(mode=0o700, exist_ok=True)
            (fixture.root / "bin/graftr").symlink_to(self.native)
        self.log = self.fixture.root.parent / "ssh.jsonl"
        self.drop = self.fixture.root.parent / "drop-ack"
        self.corrupt = self.fixture.root.parent / "corrupt"
        mappings = {"desktop": self.fixture.env, "laptop": self.other.env}
        self.routes = self.fixture.root.parent / "ssh-routes.json"
        self.routes.write_text(json.dumps(mappings))
        for fixture in (self.fixture, self.other):
            executable = fixture.root / "bin/ssh"
            executable.write_text(f'''#!/usr/bin/python3
import json, os, subprocess, sys
args = sys.argv[1:]
separator = args.index("--")
host, command = args[separator + 1:]
with open({str(self.log)!r}, "a") as output:
    output.write(json.dumps(args) + "\\n")
with open({str(self.routes)!r}) as routes:
    env = json.load(routes)[host]
data = sys.stdin.buffer.read()
if os.path.exists({str(self.corrupt)!r}) and "_transfer-receive" in command:
    data = data[:-1] + bytes([data[-1] ^ 1])
result = subprocess.run(["/bin/sh", "-c", command], input=data, env=env, capture_output=True)
if os.path.exists({str(self.drop)!r}) and "_transfer-receive" in command:
    os.unlink({str(self.drop)!r})
    sys.exit(255)
sys.stdout.buffer.write(result.stdout)
sys.stderr.buffer.write(result.stderr)
sys.exit(result.returncode)
''')
            executable.chmod(0o700)
        self.create()

    def create(self):
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"])
        self.assertEqual(result[0], 0, result)

    def call(self, fixture, *args, cwd=None):
        child = subprocess.run([str(self.native), *args, "--json"], env=fixture.env,
                               cwd=cwd or fixture.repo, capture_output=True, text=True, timeout=30)
        self.assertEqual(child.stderr, "")
        return child.returncode, json.loads(child.stdout)

    def transfer(self, fixture=None, host="laptop", extras=()):
        args = ["transfer", "bench--sample", "--to", host, "--no-herdr"]
        for extra in extras:
            args += ["--include", extra]
        return self.call(fixture or self.fixture, *args)

    def retry_follow_ups(self, fixture=None):
        return self.call(fixture or self.fixture,"transfer","bench--sample","--retry-hooks")

    def tree_git(self, fixture, *args, check=True):
        return subprocess.run(["git", "-C", str(fixture.checkout), *args], env=fixture.env,
                              check=check, capture_output=True).stdout

    def receipt(self, fixture):
        return json.loads((fixture.state / "transfers/bench--sample/receipt.json").read_text())

    def profile(self, fixture=None, **changes):
        fixture = fixture or self.fixture
        path = fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def follow_up(self, fixture=None, body="", name="board-handoff", timeout=None, args=()):
        fixture = fixture or self.fixture
        script = fixture.root / f"{name}.py"
        events = fixture.root / "follow-ups.jsonl"
        script.write_text(f'''import fcntl, hashlib, json, os, signal, sys
from pathlib import Path
root = Path({str(fixture.root)!r})
state = Path({str(fixture.state)!r})
receipt = json.loads((state / "transfers/bench--sample/receipt.json").read_text())
assert receipt["status"] == "sent"
assert receipt["result"]["status"] == "attention"
assert receipt["result"]["destinationStatus"] == "ready"
assert receipt["result"]["afterTransfer"]["hooks"][-1]["status"] == "running"
assert sys.stdin.buffer.read() == b""
with (state / "operation.lock").open("a") as lock:
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
key = hashlib.sha256(os.environ["GRAFTR_WORKTREE_PATH"].encode()).hexdigest()
with (state / ("setup-" + key + ".lock")).open("a") as lock:
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        pass
    else:
        raise AssertionError("sender checkout was not locked")
with Path({str(events)!r}).open("a") as output:
    output.write(json.dumps({{"cwd":os.getcwd(), "argv":sys.argv[1:], "env":{{key:value for key,value in os.environ.items() if key.startswith("GRAFTR_")}}}}) + "\\n")
{body}
''')
        hook = {"name":name, "command":["/usr/bin/python3",str(script),*args]}
        if timeout is not None:
            hook["timeoutMs"] = timeout
        return hook, events

    def generation(self, fixture, received=False):
        generation = self.receipt(fixture)["manifest"]["generation"]
        return fixture.state / "transfers/bench--sample" / (("received-" if received else "") + generation)

    def instrument_git(self, fixture, rules=""):
        log = fixture.root / "git-calls.jsonl"
        executable = fixture.root / "bin/git"
        executable.write_text(f'''#!/usr/bin/python3
import json, os, sys
from pathlib import Path
args = sys.argv[1:]
with open({str(log)!r}, "a") as output:
    output.write(json.dumps(args) + "\\n")
{rules}
os.execv({shutil.which("git")!r}, [{shutil.which("git")!r}, *args])
''')
        executable.chmod(0o700)
        return log

    def verify_recovery_bundle(self, fixture, bundle):
        empty = fixture.root / "empty-recovery.git"
        subprocess.run([shutil.which("git"), "init", "--quiet", "--bare", str(empty)],
                       env=fixture.env, capture_output=True, check=True)
        result = subprocess.run([shutil.which("git"), "-C", str(empty), "bundle", "verify", str(bundle)],
                                env=fixture.env, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        return empty

    def dirty(self):
        tree = self.fixture.checkout
        (tree / "tracked.txt").write_text("staged\n")
        (tree / "staged.bin").write_bytes(b"\x00\xff\x01binary")
        self.tree_git(self.fixture, "add", "tracked.txt", "staged.bin")
        (tree / "tracked.txt").write_text("unstaged\n")
        (tree / "untracked ' file.txt").write_text("not added\n")
        (tree / "run.sh").write_text("#!/bin/sh\nexit 0\n")
        (tree / "run.sh").chmod(0o755)
        (tree / "link").symlink_to("tracked.txt")

    def test_dirty_work_round_trip_preserves_source_and_rebuilds_machine_local_setup(self):
        self.dirty()
        (self.other.repo / ".env").write_text("SECRET=other-machine\n")
        config_path = self.other.root / "config/graftr/projects.json"
        config = json.loads(config_path.read_text())
        config["projects"]["bench"]["localPorts"] = [42000, 43000]
        config_path.write_text(json.dumps(config))
        source_index = self.tree_git(self.fixture, "rev-parse", "--git-path", "index").decode().strip()
        before_index = Path(source_index).read_bytes()
        before_head = self.tree_git(self.fixture, "rev-parse", "HEAD")
        before_status = self.tree_git(self.fixture, "status", "--porcelain=v1", "-z")
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["data"]["status"], "ready")
        self.assertTrue(result[1]["data"]["sourceKept"])
        self.assertEqual(Path(source_index).read_bytes(), before_index)
        self.assertEqual(self.tree_git(self.fixture, "rev-parse", "HEAD"), before_head)
        self.assertEqual(self.tree_git(self.fixture, "status", "--porcelain=v1", "-z"), before_status)
        self.assertEqual(self.tree_git(self.other, "status", "--porcelain=v1", "-z"), before_status)
        self.assertEqual(self.tree_git(self.other, "show", ":tracked.txt"), b"staged\n")
        self.assertEqual((self.other.checkout / "tracked.txt").read_text(), "unstaged\n")
        self.assertEqual((self.other.checkout / "staged.bin").read_bytes(), b"\x00\xff\x01binary")
        self.assertEqual((self.other.checkout / "link").readlink(), Path("tracked.txt"))
        self.assertEqual((self.other.checkout / "run.sh").stat().st_mode & 0o100, 0o100)
        self.assertIn("SECRET=other-machine", (self.other.checkout / ".env").read_text())
        self.assertIn("PORT=42000", (self.other.checkout / ".env").read_text())
        self.assertEqual((self.other.checkout / "node_modules/install-count").read_text(), "1\n")
        (self.other.checkout / "tracked.txt").write_text("returned\n")
        (self.other.checkout / "untracked ' file.txt").unlink()
        returned = self.transfer(self.other, "desktop")
        self.assertEqual(returned[0], 0, returned)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "returned\n")
        self.assertFalse((self.fixture.checkout / "untracked ' file.txt").exists())
        self.assertEqual(self.tree_git(self.fixture, "show", ":tracked.txt"), b"staged\n")
        self.assertIn("SECRET=synthetic-initial", (self.fixture.checkout / ".env").read_text())
        self.assertEqual((self.fixture.checkout / "node_modules/install-count").read_text(), "1\n")
        for call in self.log.read_text().splitlines():
            args = json.loads(call)
            self.assertIn("BatchMode=yes", args)
            self.assertIn("StrictHostKeyChecking=yes", args)

    def test_unpushed_commits_staged_deletions_and_renames_travel_without_snapshot_branches(self):
        self.tree_git(self.fixture, "-c", "user.name=Test", "-c", "user.email=test@example.test", "commit", "--allow-empty", "-qm", "unpushed")
        head = self.tree_git(self.fixture, "rev-parse", "HEAD")
        self.tree_git(self.fixture, "mv", "tracked.txt", "renamed.txt")
        (self.fixture.checkout / "package.json").unlink()
        self.tree_git(self.fixture, "add", "--update")
        status = self.tree_git(self.fixture, "status", "--porcelain=v1", "-z")
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        self.assertEqual(self.tree_git(self.other, "rev-parse", "HEAD"), head)
        self.assertEqual(self.tree_git(self.other, "status", "--porcelain=v1", "-z"), status)
        self.assertEqual(self.other.git("for-each-ref", "--format=%(refname)").splitlines(), ["refs/heads/main", "refs/heads/task/sample"])
        self.assertEqual(self.fixture.git("for-each-ref", "--format=%(refname)").splitlines(), ["refs/heads/main", "refs/heads/task/sample"])

    def test_explicit_ignored_extras_stay_private_and_are_selected_on_return(self):
        tree = self.fixture.checkout
        with (tree / ".gitignore").open("a") as output:
            output.write("notes.local\n")
        (tree / "notes.local").write_text("private-extra-value\n")
        (tree / ".env.local").write_text("DO_NOT_SEND=private-default\n")
        extra_oid = self.tree_git(self.fixture, "hash-object", "notes.local").strip().decode()
        result = self.transfer(extras=["notes.local"])
        self.assertEqual(result[0], 0, result)
        self.assertNotIn("private-extra-value", json.dumps(result))
        self.assertEqual((self.other.checkout / "notes.local").read_text(), "private-extra-value\n")
        self.assertEqual((self.other.checkout / "notes.local").stat().st_mode & 0o777, 0o600)
        self.assertFalse((self.other.checkout / ".env.local").exists())
        for fixture in (self.fixture, self.other):
            child = subprocess.run(["git", "-C", str(fixture.repo), "cat-file", "-e", extra_oid], env=fixture.env, capture_output=True)
            self.assertNotEqual(child.returncode, 0, "Ignored extra leaked into project Git objects")
            transfer = fixture.state / "transfers"
            self.assertEqual(transfer.stat().st_mode & 0o777, 0o700)
            self.assertEqual((transfer / "bench--sample/receipt.json").stat().st_mode & 0o777, 0o600)
        (self.other.checkout / "notes.local").write_text("updated-private-extra\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((tree / "notes.local").read_text(), "updated-private-extra\n")

    def test_independent_source_changes_refuse_return_without_overwriting_files_or_receipt(self):
        self.assertEqual(self.transfer()[0], 0)
        receipt = self.receipt(self.fixture)
        (self.fixture.checkout / "tracked.txt").write_text("independent desktop work\n")
        (self.other.checkout / "tracked.txt").write_text("laptop work\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertIn("changed since handoff", result[1]["error"]["message"])
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "independent desktop work\n")
        self.assertEqual((self.other.checkout / "tracked.txt").read_text(), "laptop work\n")
        self.assertEqual(self.receipt(self.fixture), receipt)

    def test_lost_acknowledgement_retries_identical_snapshot_without_replaying_setup(self):
        self.drop.touch()
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(self.receipt(self.fixture)["status"], "sending")
        self.assertEqual(self.receipt(self.other)["status"], "ready")
        generation = self.receipt(self.other)["manifest"]["generation"]
        (self.fixture.checkout / "newer.txt").write_text("not in the original handoff\n")
        retried = self.transfer()
        self.assertEqual(retried[0], 0, retried)
        self.assertEqual(retried[1]["data"]["generation"], generation)
        self.assertFalse((self.other.checkout / "newer.txt").exists())
        self.assertEqual((self.other.checkout / "node_modules/install-count").read_text(), "1\n")

    def test_setup_failure_acknowledges_receipt_but_reports_partial_and_never_replays_hooks(self):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["installCommand"] = ["sh", "-c", "echo attempt >> attempts; echo private-error >&2; exit 7"]
        path.write_text(json.dumps(config))
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "partial")
        self.assertEqual(result[1]["data"]["status"], "attention")
        self.assertEqual(self.receipt(self.fixture)["status"], "sent")
        self.assertTrue(self.other.checkout.exists())
        self.assertNotIn("private-error", json.dumps(result))
        self.assertEqual(self.transfer()[1]["data"], result[1]["data"])
        self.assertEqual((self.other.checkout / "attempts").read_text(), "attempt\n")

    def test_checksum_corruption_changes_no_destination_checkout_or_branch(self):
        self.corrupt.touch()
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("checksum", result[1]["error"]["message"])
        self.assertFalse(self.other.checkout.exists())
        self.assertEqual(self.other.git("branch", "--list", "task/sample"), "")
        self.assertFalse((self.other.state / "transfers/bench--sample/receipt.json").exists())

    def test_existing_destination_is_never_adopted_or_overwritten(self):
        self.other.git("worktree", "add", "-b", "task/sample", str(self.other.checkout))
        (self.other.checkout / "tracked.txt").write_text("keep\n")
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("already exists", result[1]["error"]["message"])
        self.assertEqual((self.other.checkout / "tracked.txt").read_text(), "keep\n")
        self.assertFalse((self.fixture.state / "transfers/bench--sample/receipt.json").exists())

    def test_unmanaged_linked_worktree_path_can_be_sent_without_enrolling_source(self):
        result = self.call(self.fixture, "remove", "bench--sample", "--yes")
        self.assertEqual(result[0], 0, result)
        self.fixture.git("worktree", "add", str(self.fixture.checkout), "task/sample")
        (self.fixture.checkout / "tracked.txt").write_text("unmanaged changes\n")
        result = self.call(self.fixture, "transfer", ".", "--to", "laptop", "--no-herdr", cwd=self.fixture.checkout)
        self.assertEqual(result[0], 0, result)
        self.assertFalse((self.fixture.state / "bench--sample.json").exists())
        self.assertEqual((self.other.checkout / "tracked.txt").read_text(), "unmanaged changes\n")
        (self.other.checkout / "tracked.txt").write_text("returned to unmanaged\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "returned to unmanaged\n")
        self.assertFalse((self.fixture.state / "bench--sample.json").exists())
        self.assertFalse((self.fixture.checkout / ".env").exists())
        self.assertFalse((self.fixture.checkout / "node_modules").exists())

    def test_unsafe_host_extras_and_git_states_are_refused_before_ssh(self):
        (self.fixture.checkout / "outside").symlink_to(self.other.repo, target_is_directory=True)
        for extras in [["../repo/tracked.txt"], [".git"], ["outside/tracked.txt"], ["node_modules"]]:
            result = self.transfer(extras=extras)
            self.assertEqual(result[0], 1, result)
        for host in ["-oProxyCommand=bad", "laptop;touch bad", "ssh://laptop", "$(bad)"]:
            result = self.transfer(host=host)
            self.assertEqual(result[0], 1, result)
        self.tree_git(self.fixture, "update-index", "--assume-unchanged", "tracked.txt")
        self.assertEqual(self.transfer()[0], 1)
        self.assertFalse(self.log.exists())

    def test_preserved_ignored_file_collision_blocks_return(self):
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        (self.fixture.checkout / ".env.local").write_text("desktop-only secret\n")
        (self.other.checkout / ".env.local").write_text("explicit laptop secret\n")
        result = self.transfer(self.other, "desktop", extras=[".env.local"])
        self.assertEqual(result[0], 1, result)
        self.assertIn("collides", result[1]["error"]["message"])
        self.assertEqual((self.fixture.checkout / ".env.local").read_text(), "desktop-only secret\n")
        self.assertEqual(self.receipt(self.fixture), before)

    def test_return_to_adopted_source_preserves_external_setup(self):
        self.assertEqual(self.call(self.fixture, "remove", "bench--sample", "--yes")[0], 0)
        self.fixture.git("worktree", "add", str(self.fixture.checkout), "task/sample")
        (self.fixture.checkout / ".env").write_text("EXTERNAL=keep\n")
        self.assertEqual(self.call(self.fixture, "adopt", str(self.fixture.checkout))[0], 0)
        adoption = (self.fixture.state / "adopted/bench--sample.json").read_bytes()
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        (self.other.checkout / "tracked.txt").write_text("returned adopted\n")
        (self.fixture.repo / ".env").unlink()
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        preview = self.dry_run(fixture=self.other, host="desktop")
        self.assertEqual(preview[0], 0, preview)
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "returned adopted\n")
        self.assertEqual((self.fixture.checkout / ".env").read_text(), "EXTERNAL=keep\n")
        self.assertEqual((self.fixture.state / "adopted/bench--sample.json").read_bytes(), adoption)
        self.assertFalse((self.fixture.state / "bench--sample.json").exists())
        self.assertFalse((self.fixture.checkout / "node_modules").exists())

    def test_return_keeps_original_path_after_naming_template_changes(self):
        self.assertEqual(self.transfer()[0], 0)
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["worktreePath"] = "different/{{task}}"
        path.write_text(json.dumps(config))
        (self.other.checkout / "tracked.txt").write_text("back at original path\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual(result[1]["data"]["path"], str(self.fixture.checkout))
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "back at original path\n")

    def test_raw_line_endings_and_filter_managed_bytes_are_not_transformed(self):
        tree = self.fixture.checkout
        (tree / ".gitattributes").write_text("*.txt text eol=lf\nraw.bin filter=missing-filter\n")
        (tree / "tracked.txt").write_bytes(b"literal\r\nbytes\r\n")
        (tree / "raw.bin").write_bytes(b"not a transformed filter object\x00")
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        self.assertEqual((self.other.checkout / "tracked.txt").read_bytes(), b"literal\r\nbytes\r\n")
        self.assertEqual((self.other.checkout / "raw.bin").read_bytes(), b"not a transformed filter object\x00")
        (self.other.checkout / "tracked.txt").write_bytes(b"return\r\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((tree / "tracked.txt").read_bytes(), b"return\r\n")

    def test_explicit_environment_with_wrong_machine_assignments_is_received_but_not_ready(self):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["localPorts"] = [42000, 43000]
        path.write_text(json.dumps(config))
        result = self.transfer(extras=[".env"])
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["data"]["status"], "attention")
        self.assertIn("PORT=40000", (self.other.checkout / ".env").read_text())
        self.assertEqual((self.other.checkout / ".env").stat().st_mode & 0o777, 0o600)
        self.assertFalse((self.other.checkout / "node_modules").exists())

    def test_missing_or_unrelated_destination_configuration_changes_no_destination_work(self):
        path = self.other.root / "config/graftr/projects.json"
        original = path.read_text()
        path.unlink()
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertFalse(self.other.checkout.exists())
        path.write_text(original)
        self.other.git("checkout", "--orphan", "unrelated")
        self.other.git("rm", "-rf", ".")
        (self.other.repo / "different").write_text("different history\n")
        self.other.git("add", "different")
        self.other.git("-c", "user.name=Test", "-c", "user.email=test@example.test", "commit", "-qm", "unrelated")
        self.other.git("branch", "-D", "main")
        self.other.git("reflog", "expire", "--expire=now", "--all")
        self.other.git("gc", "--prune=now")
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("does not share", result[1]["error"]["message"])
        self.assertFalse(self.other.checkout.exists())

    def test_invalid_protocol_packets_are_rejected_without_checkout_creation(self):
        for payload in [b"bad", b"GRAFTR01" + (65537).to_bytes(8, "big"), b"GRAFTR01" + (2).to_bytes(8, "big") + b"{}"]:
            result = subprocess.run([str(self.native), "_transfer-receive", "--json"], input=payload,
                                    env=self.other.env, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 1)
            self.assertFalse(self.other.checkout.exists())
            self.assertNotIn(b"panicked", result.stderr)

    def test_default_transfer_runs_destination_hooks_only_once(self):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["hooks"] = {"afterDependencies": [
            {"name": "count", "command": ["sh", "-c", "echo run >> node_modules/hook-count"]}
        ]}
        path.write_text(json.dumps(config))
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((self.other.checkout / "node_modules/hook-count").read_text(), "run\n")
        self.assertEqual((self.other.checkout / "node_modules/install-count").read_text(), "1\n")

    def test_destination_local_herdr_context_is_attached_without_replaying_setup(self):
        server = HerdrFixture(self.other.root / "transfer.sock", self.other.checkout)
        self.addCleanup(server.close)
        self.other.env.update({"HERDR_ENV": "1", "HERDR_SOCKET_PATH": server.path,
                               "HERDR_PANE_ID": "explicit-caller"})
        self.routes.write_text(json.dumps({"desktop": self.fixture.env, "laptop": self.other.env}))
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["herdr"] = "required"
        config["projects"]["bench"]["hooks"] = {"afterDependencies": [
            {"name": "count", "command": ["sh", "-c", "echo run >> node_modules/hook-count"]}
        ]}
        path.write_text(json.dumps(config))
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop")
        self.assertEqual(result[0], 0, result)
        task = json.loads((self.other.state / "bench--sample.json").read_text())
        self.assertEqual(task["socket"], server.path)
        self.assertEqual(task["workspaceId"], "owned")
        self.assertEqual((self.other.checkout / "node_modules/hook-count").read_text(), "run\n")
        self.assertEqual((self.other.checkout / "node_modules/install-count").read_text(), "1\n")
        self.assertEqual(server.errors, [])

    def test_unchanged_dirty_files_keep_inodes_mtimes_and_use_an_independent_retained_bundle(self):
        self.dirty()
        names = ["tracked.txt", "package.json", ".gitignore", "staged.bin", "run.sh", "link", "untracked ' file.txt"]
        for name in names:
            os.utime(self.fixture.checkout / name, ns=(1_600_000_000_000_000_000,) * 2, follow_symlinks=False)
        before = {name: ((self.fixture.checkout / name).lstat().st_ino,
                         (self.fixture.checkout / name).lstat().st_mtime_ns) for name in names}
        staging = self.tree_git(self.fixture, "ls-files", "--stage", "-z")
        self.assertEqual(self.transfer()[0], 0)
        original = self.generation(self.fixture) / "work.bundle"
        original_hash = hashlib.sha256(original.read_bytes()).digest()
        log = self.instrument_git(self.fixture, '''
if "bundle" in args and "create" in args and Path(args[args.index("create") + 1]).parent.name == "before":
    sys.exit(98)
''')
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        for name in names:
            stat = (self.fixture.checkout / name).lstat()
            self.assertEqual((stat.st_ino, stat.st_mtime_ns), before[name], name)
        self.assertEqual(self.tree_git(self.fixture, "ls-files", "--stage", "-z"), staging)
        received = self.generation(self.fixture, received=True)
        self.assertFalse((received / "restore-paths").exists())
        self.assertFalse((received / "checkout-index").exists())
        self.assertFalse(any("checkout-index" in json.loads(line) for line in log.read_text().splitlines()))
        backup = received / "before/work.bundle"
        self.assertEqual(hashlib.sha256(backup.read_bytes()).digest(), original_hash)
        self.assertNotEqual(backup.stat().st_ino, original.stat().st_ino)
        self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
        self.verify_recovery_bundle(self.fixture, backup)
        original.write_bytes(b"later damage to the original bundle")
        self.assertEqual(hashlib.sha256(backup.read_bytes()).digest(), original_hash)

    def test_only_changed_paths_are_written_with_literal_names_binary_data_modes_and_deletions(self):
        self.dirty()
        fixed = self.fixture.checkout / ".gitignore"
        os.utime(fixed, ns=(1_600_000_000_000_000_000,) * 2)
        before = (fixed.stat().st_ino, fixed.stat().st_mtime_ns)
        self.assertEqual(self.transfer()[0], 0)
        tree = self.other.checkout
        (tree / "tracked.txt").write_text("returned working bytes\n")
        (tree / "staged.bin").write_bytes(b"\x00new\xff\x02")
        self.tree_git(self.other, "add", "staged.bin")
        (tree / "run.sh").chmod(0o644)
        (tree / "link").unlink()
        (tree / "link").symlink_to("run.sh")
        (tree / "untracked ' file.txt").unlink()
        self.tree_git(self.other, "rm", "package.json")
        literal = "-a [x] ' file.txt"
        (tree / literal).write_bytes(b"literal filename\x00")
        expected_status = self.tree_git(self.other, "status", "--porcelain=v1", "-z")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((fixed.stat().st_ino, fixed.stat().st_mtime_ns), before)
        self.assertEqual(self.tree_git(self.fixture, "status", "--porcelain=v1", "-z"), expected_status)
        self.assertEqual(self.tree_git(self.fixture, "show", ":tracked.txt"), b"staged\n")
        self.assertEqual(self.tree_git(self.fixture, "show", ":staged.bin"), b"\x00new\xff\x02")
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "returned working bytes\n")
        self.assertEqual((self.fixture.checkout / "link").readlink(), Path("run.sh"))
        self.assertEqual((self.fixture.checkout / "run.sh").stat().st_mode & 0o100, 0)
        self.assertFalse((self.fixture.checkout / "package.json").exists())
        self.assertFalse((self.fixture.checkout / "untracked ' file.txt").exists())
        self.assertEqual((self.fixture.checkout / literal).read_bytes(), b"literal filename\x00")
        paths = (self.generation(self.fixture, received=True) / "restore-paths").read_bytes()
        self.assertEqual(set(paths.split(b"\0")) - {b""},
                         {b"tracked.txt", b"staged.bin", b"run.sh", b"link", literal.encode()})

    def test_staging_only_changes_are_restored_without_rewriting_equal_working_files(self):
        self.dirty()
        path = self.fixture.checkout / "tracked.txt"
        os.utime(path, ns=(1_600_000_000_000_000_000,) * 2)
        before = (path.stat().st_ino, path.stat().st_mtime_ns)
        self.assertEqual(self.transfer()[0], 0)
        self.tree_git(self.other, "add", "tracked.txt")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((path.stat().st_ino, path.stat().st_mtime_ns), before)
        self.assertEqual(self.tree_git(self.fixture, "show", ":tracked.txt"), b"unstaged\n")
        self.assertFalse((self.generation(self.fixture, received=True) / "restore-paths").exists())

    def test_equal_blob_bytes_with_different_regular_or_symlink_types_are_not_skipped(self):
        (self.fixture.checkout / "becomes-link").write_text("tracked.txt")
        (self.fixture.checkout / "becomes-file").symlink_to("tracked.txt")
        self.assertEqual(self.transfer()[0], 0)
        (self.other.checkout / "becomes-link").unlink()
        (self.other.checkout / "becomes-link").symlink_to("tracked.txt")
        (self.other.checkout / "becomes-file").unlink()
        (self.other.checkout / "becomes-file").write_text("tracked.txt")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertTrue((self.fixture.checkout / "becomes-link").is_symlink())
        self.assertFalse((self.fixture.checkout / "becomes-file").is_symlink())
        self.assertEqual((self.fixture.checkout / "becomes-file").read_text(), "tracked.txt")
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "committed\n")

    def test_equal_private_extra_is_replaced_to_protect_permissions_without_chmoding_a_hardlink_alias(self):
        outside = self.fixture.root / "outside-file"
        outside.write_text("explicit private extra\n")
        outside.chmod(0o755)
        with (self.fixture.checkout / ".gitignore").open("a") as file:
            file.write("notes.local\n")
        extra = self.fixture.checkout / "notes.local"
        os.link(outside, extra)
        self.assertEqual(self.transfer(extras=["notes.local"])[0], 0)
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual(extra.read_text(), "explicit private extra\n")
        self.assertEqual(extra.stat().st_mode & 0o777, 0o700)
        self.assertEqual(outside.stat().st_mode & 0o777, 0o755)
        self.assertNotEqual(extra.stat().st_ino, outside.stat().st_ino)
        self.assertEqual(extra.stat().st_nlink, 1)

    def test_missing_retained_bundle_is_regenerated_before_returning_work(self):
        self.assertEqual(self.transfer()[0], 0)
        (self.generation(self.fixture) / "work.bundle").unlink()
        (self.other.checkout / "tracked.txt").write_text("new laptop bytes\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.verify_recovery_bundle(self.fixture, self.generation(self.fixture, received=True) / "before/work.bundle")
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "new laptop bytes\n")

    def test_same_size_corrupt_retained_bundle_is_not_reused(self):
        self.assertEqual(self.transfer()[0], 0)
        original = self.generation(self.fixture) / "work.bundle"
        data = bytearray(original.read_bytes())
        data[-1] ^= 1
        original.write_bytes(data)
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        backup = self.generation(self.fixture, received=True) / "before/work.bundle"
        self.assertNotEqual(backup.read_bytes(), data)
        self.verify_recovery_bundle(self.fixture, backup)
        self.assertEqual(original.read_bytes(), data, "The corrupt artifact must be preserved, not repaired in place")

    def test_unsafe_retained_bundle_is_refused_before_checkout_or_receipt_mutation(self):
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        original = self.generation(self.fixture) / "work.bundle"
        original.unlink()
        original.symlink_to(self.fixture.repo / "tracked.txt")
        (self.other.checkout / "tracked.txt").write_text("must not overwrite source\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "committed\n")
        self.assertEqual(self.receipt(self.fixture), before)
        self.assertTrue(original.is_symlink())

    def test_nonprivate_retained_bundle_directory_is_not_bypassed_by_recovery_fallback(self):
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        self.generation(self.fixture).chmod(0o755)
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(self.receipt(self.fixture), before)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "committed\n")

    def test_failed_recovery_copy_keeps_original_checkout_and_outgoing_receipt(self):
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        original = (self.generation(self.fixture) / "work.bundle").read_bytes()
        self.instrument_git(self.fixture, '''
if "update-ref" in args:
    repo = Path(args[args.index("-C") + 1])
    if repo.name == "snapshot.git" and repo.parent.name == "before":
        (repo.parent / "work.bundle").mkdir()
''')
        (self.other.checkout / "tracked.txt").write_text("must wait for durable recovery\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(self.receipt(self.fixture), before)
        self.assertEqual((self.generation(self.fixture) / "work.bundle").read_bytes(), original)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "committed\n")

    def test_failed_selective_checkout_retains_recovery_and_is_never_blindly_replayed(self):
        self.dirty()
        self.assertEqual(self.transfer()[0], 0)
        previous = self.receipt(self.fixture)
        (self.other.checkout / "tracked.txt").write_text("new incoming bytes\n")
        log = self.instrument_git(self.fixture, '''
if "checkout-index" in args:
    sys.exit(97)
''')
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(self.receipt(self.fixture)["status"], "restoring")
        before = self.generation(self.fixture, received=True) / "before/work.bundle"
        empty = self.verify_recovery_bundle(self.fixture, before)
        subprocess.run([shutil.which("git"), "-C", str(empty), "bundle", "unbundle", str(before)],
                       env=self.fixture.env, capture_output=True, check=True)
        old_commit = previous["manifest"]["snapshot"]["commit"]
        recovered = subprocess.check_output([shutil.which("git"), "-C", str(empty), "show", old_commit + ":tracked.txt"], env=self.fixture.env)
        self.assertEqual(recovered, b"unstaged\n")
        calls = sum("checkout-index" in json.loads(line) for line in log.read_text().splitlines())
        retried = self.transfer(self.other, "desktop")
        self.assertEqual(retried[0], 1, retried)
        self.assertIn("Interrupted restoration", retried[1]["error"]["message"])
        self.assertEqual(sum("checkout-index" in json.loads(line) for line in log.read_text().splitlines()), calls)
        self.assertEqual((self.fixture.checkout / "node_modules/install-count").read_text(), "1\n")

    def test_independent_executable_mode_change_is_not_mistaken_for_an_unchanged_source(self):
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        (self.fixture.checkout / "tracked.txt").chmod(0o755)
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertIn("changed since handoff", result[1]["error"]["message"])
        self.assertEqual(self.receipt(self.fixture), before)
        self.assertEqual((self.fixture.checkout / "tracked.txt").stat().st_mode & 0o100, 0o100)

    def test_new_directory_cannot_replace_a_source_file_through_the_selective_path_list(self):
        path = self.fixture.checkout / "shape"
        path.write_text("original file\n")
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        remote = self.other.checkout / "shape"
        remote.unlink()
        remote.mkdir()
        (remote / "child").write_text("new child\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual(path.read_text(), "original file\n")
        self.assertEqual(self.receipt(self.fixture), before)

    def test_new_file_cannot_replace_a_directory_containing_preserved_local_files(self):
        path = self.fixture.checkout / "shape"
        path.mkdir()
        (path / "child").write_text("portable child\n")
        self.assertEqual(self.transfer()[0], 0)
        before = self.receipt(self.fixture)
        (path / ".env.local").write_text("private local data\n")
        remote = self.other.checkout / "shape"
        (remote / "child").unlink()
        remote.rmdir()
        remote.write_text("must not replace directory\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 1, result)
        self.assertEqual((path / "child").read_text(), "portable child\n")
        self.assertEqual((path / ".env.local").read_text(), "private local data\n")
        self.assertEqual(self.receipt(self.fixture), before)

    def test_sha256_snapshots_support_selective_restore_and_recovery_reuse(self):
        # Reinitialize only these disposable fixtures, not any real checkout.
        for fixture in (self.fixture, self.other):
            shutil.rmtree(fixture.checkout, ignore_errors=True)
            shutil.rmtree(fixture.state, ignore_errors=True)
            shutil.rmtree(fixture.repo / ".git")
            fixture.git("init", "--quiet", "--object-format=sha256", "-b", "main")
            fixture.git("add", ".")
            fixture.git("-c", "user.name=Benchmark", "-c", "user.email=benchmark@example.test", "commit", "-qm", "fixture")
            fixture.commit = fixture.git("rev-parse", "HEAD").strip()
        self.assertEqual(self.fixture.commit, self.other.commit)
        self.assertEqual(len(self.fixture.commit), 64)
        self.create()
        self.dirty()
        self.assertEqual(self.transfer()[0], 0)
        fixed = self.fixture.checkout / "package.json"
        before = (fixed.stat().st_ino, fixed.stat().st_mtime_ns)
        (self.other.checkout / "tracked.txt").write_text("sha256 return\n")
        result = self.transfer(self.other, "desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual((fixed.stat().st_ino, fixed.stat().st_mtime_ns), before)
        self.assertEqual((self.fixture.checkout / "tracked.txt").read_text(), "sha256 return\n")
        self.assertEqual(self.tree_git(self.fixture, "show", ":tracked.txt"), b"staged\n")
        backup = self.generation(self.fixture, received=True) / "before/work.bundle"
        empty = self.fixture.root / "sha256-recovery.git"
        subprocess.run([shutil.which("git"), "init", "--quiet", "--bare", "--object-format=sha256", str(empty)],
                       env=self.fixture.env, capture_output=True, check=True)
        result = subprocess.run([shutil.which("git"), "-C", str(empty), "bundle", "verify", str(backup)],
                                env=self.fixture.env, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_verification_borrows_objects_read_only_but_uses_an_independent_index(self):
        self.dirty()
        data = os.urandom(2 * 1024 * 1024)
        (self.fixture.checkout / "large.bin").write_bytes(data)
        with (self.fixture.checkout / ".gitignore").open("a") as output:
            output.write("notes.local\n")
        (self.fixture.checkout / "notes.local").write_text("private captured extra\n")
        large_oid = self.tree_git(self.fixture, "hash-object", "--no-filters", "large.bin").strip().decode()
        extra_oid = self.tree_git(self.fixture, "hash-object", "--no-filters", "notes.local").strip().decode()
        events = self.fixture.root / "capture-indexes.jsonl"
        self.instrument_git(self.fixture, f'''
if "add" in args:
    with open({str(events)!r}, "a") as output:
        output.write(json.dumps({{"index":os.environ.get("GIT_INDEX_FILE"),"objects":os.environ.get("GIT_OBJECT_DIRECTORY")}}) + "\\n")
''')
        result = self.transfer(extras=["notes.local"])
        self.assertEqual(result[0], 0, result)
        captured = self.generation(self.fixture)
        objects = captured / "snapshot.git/objects"
        verified_objects = captured / "verify/snapshot.git/objects"
        for oid in (large_oid, extra_oid):
            self.assertTrue((objects / oid[:2] / oid[2:]).is_file())
            self.assertFalse((verified_objects / oid[:2] / oid[2:]).exists(), "Verification wrote a duplicate blob")
            check = subprocess.run([shutil.which("git"), "-C", str(self.fixture.repo), "cat-file", "-e", oid],
                                   env=self.fixture.env, capture_output=True)
            self.assertNotEqual(check.returncode, 0, "Private captured bytes leaked into project Git objects")
        alternates = (verified_objects / "info/alternates").read_text().splitlines()
        self.assertEqual(alternates[0], str(objects.resolve()))
        self.assertNotEqual((captured / "index").stat().st_ino, (captured / "verify/index").stat().st_ino)
        observed = [json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual({row["index"] for row in observed}, {str(captured / "index"), str(captured / "verify/index")})
        self.assertEqual({row["objects"] for row in observed}, {str(objects), str(verified_objects)})
        self.assertEqual((self.other.checkout / "large.bin").read_bytes(), data)
        self.assertNotIn("private captured extra", json.dumps(result))
        shutil.rmtree(captured / "verify")
        bundle = captured / "work.bundle"
        empty = self.verify_recovery_bundle(self.fixture, bundle)
        subprocess.run([shutil.which("git"), "-C", str(empty), "bundle", "unbundle", str(bundle)],
                       env=self.fixture.env, capture_output=True, check=True)
        commit = self.receipt(self.fixture)["manifest"]["snapshot"]["commit"]
        recovered = subprocess.check_output([shutil.which("git"), "-C", str(empty), "show", commit + ":large.bin"], env=self.fixture.env)
        self.assertEqual(recovered, data, "The exported bundle depends on verification storage")

    def test_verification_detects_same_size_same_mtime_edits_instead_of_trusting_the_first_index(self):
        self.dirty()
        self.fixture.env.update({"GIT_CONFIG_COUNT":"1", "GIT_CONFIG_KEY_0":"core.trustctime", "GIT_CONFIG_VALUE_0":"false"})
        index = Path(self.tree_git(self.fixture, "rev-parse", "--git-path", "index").decode().strip())
        before_index = index.read_bytes()
        tracked = self.fixture.checkout / "tracked.txt"
        untracked = self.fixture.checkout / "untracked ' file.txt"
        old_times = {str(path):path.stat().st_mtime_ns for path in (tracked,untracked)}
        self.instrument_git(self.fixture, f'''
if "init" in args and Path(args[-1]).parts[-2:] == ("verify","snapshot.git"):
    for name, data in [({str(tracked)!r},b"changed!\\n"),({str(untracked)!r},b"new bytes\\n")]:
        path=Path(name); before=path.stat()
        assert len(data)==before.st_size
        path.write_bytes(data)
        os.utime(path,ns=(before.st_atime_ns,before.st_mtime_ns))
''')
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("changed while being captured", result[1]["error"]["message"])
        self.assertEqual(tracked.read_bytes(), b"changed!\n")
        self.assertEqual(untracked.read_bytes(), b"new bytes\n")
        for name, mtime in old_times.items():
            self.assertEqual(Path(name).stat().st_mtime_ns, mtime)
        self.assertEqual(index.read_bytes(), before_index)
        self.assertFalse(self.other.checkout.exists())
        self.assertFalse((self.fixture.state / "transfers/bench--sample/receipt.json").exists())
        failed = next(step for step in result[1]["progress"] if step["action"] == "verify-transfer")
        self.assertEqual(failed["status"], "uncertain")
        self.assertGreaterEqual(failed["elapsedMs"], 0)
        self.assertFalse(any("_transfer-receive" in line for line in self.log.read_text().splitlines()))

    def test_verification_refuses_a_nonprivate_first_snapshot_store(self):
        self.dirty()
        self.instrument_git(self.fixture, '''
if "init" in args and Path(args[-1]).parts[-2:] == ("verify","snapshot.git"):
    (Path(args[-1]).parents[1] / "snapshot.git").chmod(0o755)
''')
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("private", result[1]["error"]["message"])
        self.assertFalse(self.other.checkout.exists())
        self.assertFalse((self.fixture.state / "transfers/bench--sample/receipt.json").exists())

    def test_verification_refuses_a_symlinked_borrowed_object_directory(self):
        (self.fixture.checkout / "untracked-private.bin").write_bytes(b"private bytes never staged\x00")
        oid = self.tree_git(self.fixture, "hash-object", "untracked-private.bin").strip().decode()
        self.instrument_git(self.fixture, f'''
if "init" in args and Path(args[-1]).parts[-2:] == ("verify","snapshot.git"):
    private=Path(args[-1]).parents[1] / "snapshot.git"
    (private/"objects").rename(private/"original-objects")
    (private/"objects").symlink_to({str(self.fixture.repo / '.git/objects')!r},target_is_directory=True)
''')
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("symlink", result[1]["error"]["message"])
        self.assertFalse(self.other.checkout.exists())
        self.assertFalse((self.fixture.state / "transfers/bench--sample/receipt.json").exists())
        check = subprocess.run([shutil.which("git"), "-C", str(self.fixture.repo), "cat-file", "-e", oid], env=self.fixture.env, capture_output=True)
        self.assertNotEqual(check.returncode, 0)

    def test_verification_reuse_supports_state_paths_containing_colons(self):
        old = self.fixture.state
        self.fixture.state = self.fixture.root / "state:private/graftr"
        self.fixture.state.parent.mkdir(mode=0o700)
        old.rename(self.fixture.state)
        self.fixture.env["XDG_STATE_HOME"] = str(self.fixture.state.parent)
        self.routes.write_text(json.dumps({"desktop":self.fixture.env, "laptop":self.other.env}))
        self.dirty()
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        captured = self.generation(self.fixture)
        self.assertIn(str((captured / "snapshot.git/objects").resolve()),
                      (captured / "verify/snapshot.git/objects/info/alternates").read_text().splitlines())
        self.assertEqual(self.transfer(self.other,"desktop")[0], 0)

    def test_transfer_reports_monotonic_stage_timings_and_keeps_cached_acknowledgements_stable(self):
        result = self.transfer()
        self.assertEqual(result[0], 0, result)
        sender = result[1]["data"]["timings"]
        receiver = sender["destination"]
        for report, side in ((sender,"sender"),(receiver,"receiver")):
            self.assertEqual(report["side"],side)
            self.assertTrue(math.isfinite(report["elapsedMs"]))
            self.assertGreaterEqual(report["elapsedMs"],0)
            for duration in report["stagesMs"].values():
                self.assertTrue(math.isfinite(duration))
                self.assertGreaterEqual(duration,0)
            self.assertLessEqual(sum(report["stagesMs"].values()),report["elapsedMs"]+1)
        self.assertTrue({"capture-transfer","verify-transfer","bundle-transfer","send-transfer"} <= sender["stagesMs"].keys())
        self.assertTrue({"receive-transfer","unpack-transfer","restore-transfer-files","prepare-transfer-destination"} <= receiver["stagesMs"].keys())
        for step in result[1]["progress"]:
            if "elapsedMs" in step:
                self.assertTrue(math.isfinite(step["elapsedMs"]))
                self.assertGreaterEqual(step["elapsedMs"],0)
        self.assertEqual(self.transfer()[1]["data"],result[1]["data"])
        human = subprocess.run([str(self.native),"transfer","bench--sample","--to","laptop","--no-herdr"],
                               env=self.fixture.env,cwd=self.fixture.repo,capture_output=True,text=True,timeout=10)
        self.assertEqual(human.returncode,0,human.stderr)
        self.assertIn("Recorded timing:",human.stdout)
        self.assertIn("transfer/setup",human.stdout)

    def test_retry_accepts_legacy_acknowledgements_without_destination_timings(self):
        self.drop.touch()
        self.assertEqual(self.transfer()[0],1)
        path = self.other.state / "transfers/bench--sample/receipt.json"
        receipt = json.loads(path.read_text())
        receipt["result"].pop("timings")
        path.write_text(json.dumps(receipt))
        result = self.transfer()
        self.assertEqual(result[0],0,result)
        timings = result[1]["data"]["timings"]
        self.assertNotIn("destination",timings)
        self.assertNotIn("capture-transfer",timings["stagesMs"])
        self.assertIn("send-transfer",timings["stagesMs"])
        source = self.fixture.state / "transfers/bench--sample/receipt.json"
        receipt = json.loads(source.read_text())
        receipt["result"].pop("timings")
        source.write_text(json.dumps(receipt))
        legacy = self.transfer()
        self.assertEqual(legacy[0],0,legacy)
        self.assertNotIn("timings",legacy[1]["data"])

    def test_partial_setup_results_retain_destination_stage_timings(self):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"]["installCommand"] = ["sh","-c","exit 7"]
        path.write_text(json.dumps(config))
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"]["status"],"attention")
        timing = result[1]["data"]["timings"]["destination"]["stagesMs"]["prepare-transfer-destination"]
        self.assertTrue(math.isfinite(timing))
        self.assertGreaterEqual(timing,0)

    def test_clean_verification_keeps_the_normal_object_lookup_path_and_a_fresh_index(self):
        result = self.transfer()
        self.assertEqual(result[0],0,result)
        captured = self.generation(self.fixture)
        snapshot = self.receipt(self.fixture)["manifest"]["snapshot"]
        self.assertEqual(snapshot["worktreeTree"],snapshot["indexTree"])
        alternates = (captured / "verify/snapshot.git/objects/info/alternates").read_text().splitlines()
        self.assertNotIn(str((captured / "snapshot.git/objects").resolve()),alternates)
        self.assertNotEqual((captured / "index").stat().st_ino,(captured / "verify/index").stat().st_ino)
        self.assertIn("verify-transfer",result[1]["data"]["timings"]["stagesMs"])

    def test_initially_clean_work_is_still_rechecked_without_object_borrowing(self):
        path = self.fixture.checkout / "tracked.txt"
        self.fixture.env.update({"GIT_CONFIG_COUNT":"1", "GIT_CONFIG_KEY_0":"core.trustctime", "GIT_CONFIG_VALUE_0":"false"})
        self.instrument_git(self.fixture, f'''
if "init" in args and Path(args[-1]).parts[-2:] == ("verify","snapshot.git"):
    path=Path({str(path)!r}); before=path.stat()
    data=b"different\\n"
    assert len(data)==before.st_size
    path.write_bytes(data)
    os.utime(path,ns=(before.st_atime_ns,before.st_mtime_ns))
''')
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertIn("changed while being captured",result[1]["error"]["message"])
        self.assertEqual(path.read_bytes(),b"different\n")
        self.assertFalse(self.other.checkout.exists())

    def test_after_transfer_runs_on_sender_only_with_literal_argv_and_authoritative_context(self):
        hook, events = self.follow_up(body='print("private-board-command-output")',
                                     args=("$GRAFTR_TRANSFER_DESTINATION", "literal;not-a-shell"))
        other_hook, other_events = self.follow_up(self.other)
        self.profile(hooks={
            "afterTransfer":[hook],
            "afterDependencies":[{"name":"no-stale-transfer","command":["sh","-c",'test -z "$GRAFTR_TRANSFER_ID" && test -z "$GRAFTR_TRANSFER_UNKNOWN" && test "$GRAFTR_SETUP_RUN_ID" != stale']}],
        })
        self.profile(self.other,hooks={"afterTransfer":[other_hook]})
        stale = {key:"stale" for key in (
            "GRAFTR_PROJECT","GRAFTR_TASK_ID","GRAFTR_WORKTREE_PATH","GRAFTR_BRANCH",
            "GRAFTR_HOOK_PHASE","GRAFTR_HOOK_NAME","GRAFTR_TRANSFER_ID",
            "GRAFTR_TRANSFER_DESTINATION","GRAFTR_TRANSFER_DESTINATION_PATH",
            "GRAFTR_TRANSFER_UNKNOWN","GRAFTR_SETUP_RUN_ID","GRAFTR_DEPENDENCIES",
            "GRAFTR_CLEANUP_RUN_ID","GRAFTR_RETIRE_RUN_ID","GRAFTR_TASK_SLUG",
            "GRAFTR_LOCAL_PORT","GRAFTR_HTTPS_PORT","GRAFTR_URL","GRAFTR_PORT_STALE")}
        self.fixture.env.update(stale)
        # The transfer-only phase never becomes an ordinary setup hook.
        prepared = self.call(self.fixture,"setup","--no-herdr",cwd=self.fixture.checkout)
        self.assertEqual(prepared[0],0,prepared)
        self.assertFalse(events.exists())
        result = self.transfer()
        self.assertEqual(result[0],0,result)
        data = result[1]["data"]
        self.assertEqual(data["afterTransfer"]["status"],"complete")
        observed = json.loads(events.read_text())
        self.assertEqual(observed["cwd"],str(self.fixture.checkout))
        self.assertEqual(observed["argv"],["$GRAFTR_TRANSFER_DESTINATION","literal;not-a-shell"])
        self.assertEqual(observed["env"],{
            "GRAFTR_PROJECT":"bench", "GRAFTR_TASK_ID":"bench--sample",
            "GRAFTR_WORKTREE_PATH":str(self.fixture.checkout), "GRAFTR_BRANCH":"task/sample",
            "GRAFTR_HOOK_PHASE":"afterTransfer", "GRAFTR_HOOK_NAME":"board-handoff",
            "GRAFTR_TRANSFER_ID":data["generation"],
            "GRAFTR_TRANSFER_LINEAGE":self.receipt(self.fixture)["manifest"]["lineage"],
            "GRAFTR_TRANSFER_DESTINATION":"laptop",
            "GRAFTR_TRANSFER_DESTINATION_PATH":str(self.other.checkout),
        })
        self.assertFalse(other_events.exists())
        observation = data["afterTransfer"]["hooks"][0]
        self.assertEqual(observation["status"],"succeeded")
        self.assertEqual(Path(observation["log"]).stat().st_mode & 0o777,0o600)
        self.assertIn("private-board-command-output",Path(observation["log"]).read_text())
        self.assertNotIn("private-board-command-output",json.dumps(result))
        self.assertNotIn("private-board-command-output",json.dumps(self.receipt(self.fixture)))
        self.assertIn("run-after-transfer-hook",data["timings"]["stagesMs"])
        self.assertEqual(self.transfer()[1]["data"],data)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_after_transfer_runs_once_per_generation_in_both_directions(self):
        for fixture in (self.fixture,self.other):
            hook,_ = self.follow_up(fixture)
            self.profile(fixture,hooks={"afterTransfer":[hook]})
        sent = self.transfer()
        returned = self.transfer(self.other,"desktop")
        again = self.transfer()
        for result in (sent,returned,again):
            self.assertEqual(result[0],0,result)
            self.assertEqual(result[1]["data"]["afterTransfer"]["status"],"complete")
        source = [json.loads(row) for row in (self.fixture.root/"follow-ups.jsonl").read_text().splitlines()]
        other = [json.loads(row) for row in (self.other.root/"follow-ups.jsonl").read_text().splitlines()]
        self.assertEqual(len(source),2)
        self.assertEqual(len(other),1)
        self.assertEqual(other[0]["env"]["GRAFTR_TRANSFER_DESTINATION"],"desktop")
        self.assertEqual(other[0]["env"]["GRAFTR_TRANSFER_DESTINATION_PATH"],str(self.fixture.checkout))
        self.assertNotEqual(source[0]["env"]["GRAFTR_TRANSFER_ID"],source[1]["env"]["GRAFTR_TRANSFER_ID"])
        self.assertEqual(source[0]["env"]["GRAFTR_TRANSFER_LINEAGE"],source[1]["env"]["GRAFTR_TRANSFER_LINEAGE"])

    def test_after_transfer_waits_for_confirmation_after_a_lost_acknowledgement(self):
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        self.drop.touch()
        self.assertEqual(self.transfer()[0],1)
        self.assertFalse(events.exists())
        self.assertEqual(self.receipt(self.fixture)["status"],"sending")
        confirmed = self.transfer()
        self.assertEqual(confirmed[0],0,confirmed)
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual(self.transfer()[1]["data"],confirmed[1]["data"])
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual((self.other.checkout/"node_modules/install-count").read_text(),"1\n")

    def transfer_snapshot(self, fixture):
        files = {}
        for directory in (fixture.repo, fixture.root / "trees"):
            for path in [directory, *directory.rglob("*")]:
                info = path.lstat()
                content = os.readlink(path) if path.is_symlink() else hashlib.sha256(path.read_bytes()).hexdigest() if path.is_file() else None
                files[str(path)] = (info.st_ino, info.st_mode, info.st_mtime_ns, content)
        return bench.readonly_snapshot(fixture), files

    def dry_run(self, *args, fixture=None, host="laptop"):
        return self.call(fixture or self.fixture, "transfer", "bench--sample", "--to", host,
                         "--no-herdr", "--dry-run", *args)

    def missing_project(self, **overrides):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        del config["projects"]["bench"]
        path.write_text(json.dumps(config))
        overlay = self.other.root / "config/graftr/destination.json"
        overlay.write_text(json.dumps(overrides))
        overlay.chmod(0o600)
        return overlay

    def test_transfer_dry_run_checks_dirty_work_without_writing_either_machine(self):
        self.dirty()
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 0, result)
        self.assertTrue(result[1]["data"]["dryRun"])
        self.assertTrue(result[1]["data"]["canTransfer"])
        self.assertIn("not-checked", [f["status"] for f in result[1]["data"]["findings"]])
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)
        self.assertFalse(any("_transfer-receive" in line for line in self.log.read_text().splitlines()))
        self.assertFalse((self.fixture.state / "transfers").exists())

    def test_transfer_dry_run_reports_destination_collision_as_readonly_failure(self):
        self.other.git("worktree", "add", "-b", "task/sample", str(self.other.checkout))
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 1, result)
        self.assertEqual(result[1]["outcome"], "failed")
        self.assertFalse(result[1]["data"]["canTransfer"])
        self.assertIn("already exists", json.dumps(result[1]["data"]["findings"]))
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_transfer_dry_run_validates_literal_return_snapshot_and_detects_same_mtime_edits(self):
        self.dirty()
        self.assertEqual(self.transfer()[0], 0)
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run(fixture=self.other, host="desktop")
        self.assertEqual(result[0], 0, result)
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)
        path = self.fixture.checkout / "tracked.txt"
        info = path.stat()
        path.write_bytes(b"different\n")
        os.utime(path, ns=(info.st_atime_ns, info.st_mtime_ns))
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run(fixture=self.other, host="desktop")
        self.assertEqual(result[0], 1, result)
        self.assertIn("changed since handoff", json.dumps(result[1]["data"]["findings"]))
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_transfer_dry_run_checks_baseline_and_installer_without_running_commands(self):
        self.profile(self.other, installCommand=["missing-graftr-installer"])
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 1, result)
        self.assertIn("installer executable is unavailable", json.dumps(result[1]["data"]["findings"]))
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)
        self.profile(self.other, installCommand=["sh", "-c", "touch should-not-run"])
        (self.other.repo / ".env").unlink()
        result = self.dry_run()
        self.assertEqual(result[0], 1, result)
        self.assertIn("baseline is missing", json.dumps(result[1]["data"]["findings"]))
        self.assertFalse((self.other.repo / "should-not-run").exists())

    def test_missing_project_preview_suggests_local_settings_without_copying_source_values(self):
        path = self.other.root / "config/graftr/projects.json"
        path.unlink()
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 1, result)
        migration = result[1]["data"]["projectMigration"]
        self.assertEqual(migration["local"]["repo"], str(self.other.root / "home/projects/bench"))
        self.assertEqual(migration["local"]["worktreeRoot"], str(self.other.root / "home/worktrees/bench"))
        self.assertEqual(migration["local"]["localPorts"], [32000, 32100])
        self.assertIn("installCommand", migration["portableFields"])
        self.assertIn("baseline", " ".join(migration["blocked"]))
        self.assertRegex(migration["approval"], r"^[0-9a-f]{64}$")
        self.assertIn("--approve-project", result[1]["data"]["approvalCommand"])
        self.assertNotIn(str(self.fixture.root), json.dumps(migration))
        self.assertNotIn("synthetic-initial", json.dumps(result))
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_missing_project_transfer_requires_approval_and_keeps_other_configuration(self):
        self.missing_project()
        before = self.transfer_snapshot(self.other)
        result = self.transfer()
        self.assertEqual(result[0], 1, result)
        self.assertIn("projectMigration", result[1]["data"])
        self.assertEqual(self.transfer_snapshot(self.other), before)
        self.assertFalse((self.other.root / "home/projects").exists())

    def test_approved_project_migration_clones_main_branch_then_transfers_dirty_work(self):
        self.dirty()
        self.fixture.git("remote", "add", "origin", "https://private-token@example.invalid/source.git")
        self.profile(hooks={"afterDependencies": [{"name": "generated", "command": ["sh", "-c", "printf done > generated"]}]})
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        config_path = self.other.root / "config/graftr/projects.json"
        config = json.loads(config_path.read_text())
        config["projects"]["keep"] = {"repo": str(self.other.repo), "worktreeRoot": str(self.other.root / "keep"), "localPorts": [32000, 32100]}
        config["customTopLevel"] = {"keep": True}
        config_path.write_text(json.dumps(config))
        result = self.dry_run("--destination-config", str(overlay))
        self.assertEqual(result[0], 1, result)
        migration = result[1]["data"]["projectMigration"]
        self.assertEqual(migration["blocked"], [])
        self.assertEqual(migration["local"]["localPorts"], [32100, 32200])
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--no-herdr",
                           "--destination-config", str(overlay), "--approve-project", migration["approval"])
        self.assertEqual(result[0], 0, result)
        received = Path(result[1]["data"]["path"])
        self.assertEqual((received / "tracked.txt").read_text(), "unstaged\n")
        self.assertEqual((received / "staged.bin").read_bytes(), b"\x00\xff\x01binary")
        self.assertEqual((received / "generated").read_text(), "done")
        self.assertEqual((received / "node_modules/install-count").read_text(), "1\n")
        new_config = json.loads(config_path.read_text())
        self.assertEqual(new_config["projects"]["keep"], config["projects"]["keep"])
        self.assertEqual(new_config["customTopLevel"], config["customTopLevel"])
        self.assertFalse(new_config["startupThaw"])
        self.assertEqual(new_config["herdr"], "off")
        new_repo = new_config["projects"]["bench"]["repo"]
        self.assertEqual(self.other.git("-C", new_repo, "remote"), "")
        self.assertEqual(self.other.git("-C", new_repo, "branch", "--show-current").strip(), "main")
        self.assertNotIn("private-token", json.dumps(result))
        self.assertEqual(config_path.stat().st_mode & 0o777, 0o600)

    def test_approved_project_migration_can_configure_an_existing_clone_without_changing_it(self):
        overlay = self.missing_project(repo=str(self.other.repo), worktreeRoot=str(self.other.root / "trees"), envSource=str(self.other.repo / ".env"))
        self.other.git("remote", "add", "origin", "https://destination.invalid/keep.git")
        (self.other.repo / "tracked.txt").write_text("main-checkout local work\n")
        index = (self.other.repo / ".git/index").read_bytes()
        result = self.dry_run("--destination-config", str(overlay))
        migration = result[1]["data"]["projectMigration"]
        self.assertTrue(migration["repoExists"])
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--no-herdr",
                           "--destination-config", str(overlay), "--approve-project", migration["approval"])
        self.assertEqual(result[0], 0, result)
        self.assertEqual((self.other.repo / "tracked.txt").read_text(), "main-checkout local work\n")
        self.assertEqual((self.other.repo / ".git/index").read_bytes(), index)
        self.assertEqual(self.other.git("remote", "get-url", "origin").strip(), "https://destination.invalid/keep.git")

    def test_project_migration_approval_is_invalidated_by_destination_config_changes(self):
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        result = self.dry_run("--destination-config", str(overlay))
        approval = result[1]["data"]["projectMigration"]["approval"]
        overlay.write_text(json.dumps({"envSource": str(self.other.repo / ".env"), "localPorts": [45000, 45100]}))
        before = self.transfer_snapshot(self.other)
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--no-herdr",
                           "--destination-config", str(overlay), "--approve-project", approval)
        self.assertEqual(result[0], 1, result)
        self.assertIn("proposal changed", result[1]["error"]["message"])
        self.assertEqual(self.transfer_snapshot(self.other), before)

    def test_project_migration_cannot_approve_missing_environment_or_replace_existing_project(self):
        overlay = self.missing_project()
        result = self.dry_run("--destination-config", str(overlay))
        approval = result[1]["data"]["projectMigration"]["approval"]
        before = self.transfer_snapshot(self.other)
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--no-herdr",
                           "--destination-config", str(overlay), "--approve-project", approval)
        self.assertEqual(result[0], 1, result)
        self.assertIn("blocked local settings", result[1]["error"]["message"])
        self.assertEqual(self.transfer_snapshot(self.other), before)
        config_path = self.other.root / "config/graftr/projects.json"
        config = json.loads(config_path.read_text())
        profile = json.loads((self.fixture.root / "config/graftr/projects.json").read_text())["projects"]["bench"]
        profile.update(repo=str(self.other.repo), worktreeRoot=str(self.other.root / "trees"), envSource=str(self.other.repo / ".env"))
        config["projects"]["bench"] = profile
        config_path.write_text(json.dumps(config))
        before = self.transfer_snapshot(self.other)
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--approve-project", approval)
        self.assertEqual(result[0], 1, result)
        self.assertIn("already exists", result[1]["error"]["message"])
        self.assertEqual(self.transfer_snapshot(self.other), before)

    def test_project_migration_requires_replacements_for_embedded_source_paths(self):
        self.profile(hooks={"afterDependencies": [{"name": "local", "command": ["sh", str(self.fixture.root / "private-script.sh")]}]})
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        result = self.dry_run("--destination-config", str(overlay))
        migration = result[1]["data"]["projectMigration"]
        self.assertNotIn("hooks", migration["portableFields"])
        self.assertIn("Source hooks", " ".join(migration["blocked"]))
        self.assertNotIn(str(self.fixture.root), json.dumps(migration))
        overlay.write_text(json.dumps({"envSource": str(self.other.repo / ".env"), "hooks": {}}))
        result = self.dry_run("--destination-config", str(overlay))
        self.assertEqual(result[1]["data"]["projectMigration"]["blocked"], [])

    def test_transfer_dry_run_after_lost_ack_uses_saved_snapshot_without_replaying_setup(self):
        self.drop.touch()
        self.assertEqual(self.transfer()[0], 1)
        (self.fixture.checkout / "newer.txt").write_text("not part of the saved transfer\n")
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 0, result)
        self.assertTrue(result[1]["data"]["retry"])
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_transfer_new_flags_reject_conflicting_or_unrelated_commands_before_ssh(self):
        for args in (["setup", "--dry-run"], ["transfer", "--retry-hooks", "--dry-run"],
                     ["transfer", "--to", "laptop", "--dry-run", "--approve-project", "a" * 64],
                     ["transfer", "--to", "laptop", "--approve-project", "not-an-approval"]):
            result = self.call(self.fixture, *args)
            self.assertEqual(result[0], 1, result)
            self.assertEqual(result[1]["error"]["phase"], "arguments")
        self.assertFalse(self.log.exists())

    def test_project_migration_overlay_is_private_validated_and_cannot_follow_symlinks(self):
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        original = overlay.read_text()
        overlay.chmod(0o644)
        before = self.transfer_snapshot(self.other)
        self.assertEqual(self.dry_run("--destination-config", str(overlay))[0], 1)
        self.assertEqual(self.transfer_snapshot(self.other), before)
        overlay.chmod(0o600)
        overlay.write_text(json.dumps({"unsupported": "private-overlay-value"}))
        result = self.dry_run("--destination-config", str(overlay))
        self.assertEqual(result[0], 1, result)
        self.assertNotIn("private-overlay-value", json.dumps(result))
        overlay.write_text(original)
        link = overlay.with_name("link.json")
        link.symlink_to(overlay)
        self.assertEqual(self.dry_run("--destination-config", str(link))[0], 1)
        alias = self.other.root / "home/alias"
        alias.symlink_to(self.other.root / "trees", target_is_directory=True)
        overlay.write_text(json.dumps({"repo": str(alias / "project"), "envSource": str(self.other.repo / ".env")}))
        before = self.transfer_snapshot(self.other)
        result = self.dry_run("--destination-config", str(overlay))
        self.assertEqual(result[0], 1, result)
        self.assertEqual(self.transfer_snapshot(self.other), before)

    def test_project_migration_preserves_port_names_but_requires_a_destination_url(self):
        self.profile(httpsPorts=[61000, 61100], ports={"api": [61200, 61300]},
                     url="https://source-machine.invalid:{{httpsPort}}")
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        result = self.dry_run("--destination-config", str(overlay))
        migration = result[1]["data"]["projectMigration"]
        self.assertEqual(migration["local"]["httpsPorts"], [32100, 32200])
        self.assertEqual(migration["local"]["ports"]["api"], [32200, 32300])
        self.assertNotIn("source-machine", json.dumps(migration))
        self.assertIn("destination's URL", " ".join(migration["blocked"]))

    def test_project_migration_preview_checks_the_proposed_task_path_before_approval(self):
        overlay = self.missing_project(repo=str(self.other.repo), worktreeRoot=str(self.other.root / "trees"), envSource=str(self.other.repo / ".env"))
        self.other.git("worktree", "add", "-b", "task/sample", str(self.other.checkout))
        before = self.transfer_snapshot(self.other)
        result = self.dry_run("--destination-config", str(overlay))
        self.assertEqual(result[0], 1, result)
        self.assertIn("already exists", " ".join(result[1]["data"]["projectMigration"]["blocked"]))
        self.assertEqual(self.transfer_snapshot(self.other), before)

    def test_transfer_dry_run_requires_explicit_standalone_opt_in_when_herdr_context_is_missing(self):
        path = self.other.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["herdr"] = "required"
        path.write_text(json.dumps(config))
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--dry-run")
        self.assertEqual(result[0], 1, result)
        self.assertIn("requires Herdr", json.dumps(result[1]["data"]["findings"]))
        self.assertEqual(self.dry_run()[0], 0)
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_transfer_dry_run_does_not_claim_an_ordinary_retry_repairs_failed_followups(self):
        hook, events = self.follow_up(body="raise SystemExit(7)")
        self.profile(hooks={"afterTransfer": [hook]})
        self.assertEqual(self.transfer()[0], 1)
        before = [self.transfer_snapshot(f) for f in (self.fixture, self.other)]
        result = self.dry_run()
        self.assertEqual(result[0], 1, result)
        self.assertFalse(result[1]["data"]["canTransfer"])
        self.assertIn("sender recorded", json.dumps(result[1]["data"]["findings"]))
        self.assertEqual(len(events.read_text().splitlines()), 1)
        self.assertEqual([self.transfer_snapshot(f) for f in (self.fixture, self.other)], before)

    def test_project_migration_rejects_corrupt_seed_before_cloning_or_writing_configuration(self):
        overlay = self.missing_project(envSource=str(self.other.repo / ".env"))
        result = self.dry_run("--destination-config", str(overlay))
        migration = result[1]["data"]["projectMigration"]
        executable = self.fixture.root / "bin/ssh"
        executable.write_text(executable.read_text().replace('and "_transfer-receive" in command:',
            'and ("_transfer-receive" in command or "_transfer-project-receive" in command):'))
        self.corrupt.touch()
        config_path = self.other.root / "config/graftr/projects.json"
        config_before = config_path.read_bytes()
        result = self.call(self.fixture, "transfer", "bench--sample", "--to", "laptop", "--no-herdr",
                           "--destination-config", str(overlay), "--approve-project", migration["approval"])
        self.assertEqual(result[0], 1, result)
        self.assertIn("checksum", result[1]["error"]["message"])
        self.assertEqual(config_path.read_bytes(), config_before)
        self.assertFalse(Path(migration["local"]["repo"]).exists())
        self.assertFalse(Path(migration["local"]["worktreeRoot"]).exists())

    def test_after_transfer_failure_keeps_destination_ready_and_never_replays_or_runs_later_hooks(self):
        first,events = self.follow_up(body='print("private-hook-error",file=sys.stderr);sys.exit(7)')
        later,later_events = self.follow_up(name="later")
        self.profile(hooks={"afterTransfer":[first,later]})
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        data = result[1]["data"]
        self.assertEqual(result[1]["outcome"],"partial")
        self.assertEqual(result[1]["error"]["code"],"TRANSFER_ATTENTION")
        self.assertEqual(data["status"],"attention")
        self.assertEqual(data["destinationStatus"],"ready")
        self.assertEqual(data["afterTransfer"]["status"],"failed")
        self.assertEqual(len(data["afterTransfer"]["hooks"]),1)
        self.assertEqual(data["afterTransfer"]["hooks"][0]["exitCode"],7)
        self.assertNotIn("private-hook-error",json.dumps(result))
        self.assertNotIn("private-hook-error",json.dumps(self.receipt(self.fixture)))
        self.assertEqual(self.receipt(self.other)["status"],"ready")
        self.assertEqual(self.transfer()[1]["data"],data)
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual(events,later_events)
        self.assertEqual(result[1]["progress"][-1]["status"],"uncertain")
        human = subprocess.run([str(self.native),"transfer","bench--sample","--to","laptop","--no-herdr"],
                               env=self.fixture.env,cwd=self.fixture.repo,capture_output=True,text=True,timeout=10)
        self.assertEqual(human.returncode,1)
        self.assertIn("sender afterTransfer hooks need attention",human.stdout)
        self.assertNotIn("private-hook-error",human.stdout+human.stderr)

    def test_after_transfer_timeout_is_bounded_and_preserves_the_confirmed_handoff(self):
        hook,events = self.follow_up(body='import time;time.sleep(60)',timeout=100)
        self.profile(hooks={"afterTransfer":[hook]})
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertTrue(events.exists())
        self.assertEqual(result[1]["data"]["afterTransfer"]["hooks"][0]["reason"],"timeout")
        self.assertEqual(self.receipt(self.other)["status"],"ready")
        self.assertEqual(self.transfer()[1]["data"],result[1]["data"])

    def test_missing_after_transfer_executable_reports_attention_without_replaying(self):
        self.profile(hooks={"afterTransfer":[{"name":"missing","command":["graftr-deliberately-missing-hook"]}]})
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"]["afterTransfer"]["hooks"][0]["reason"],"command-unavailable")
        self.assertEqual(self.receipt(self.other)["status"],"ready")
        self.assertEqual(self.transfer()[1]["data"],result[1]["data"])

    def test_interrupted_after_transfer_records_uncertainty_and_does_not_replay_successful_or_running_hooks(self):
        first,events = self.follow_up(name="first")
        interrupted,_ = self.follow_up(name="interrupted",body='os.kill(os.getppid(),signal.SIGKILL)')
        later,_ = self.follow_up(name="later")
        self.profile(hooks={"afterTransfer":[first,interrupted,later]})
        child = subprocess.run([str(self.native),"transfer","bench--sample","--to","laptop","--no-herdr","--json"],
                               env=self.fixture.env,cwd=self.fixture.repo,capture_output=True,text=True,timeout=30)
        self.assertEqual(child.returncode,-9,child.stdout+child.stderr)
        receipt = self.receipt(self.fixture)
        self.assertEqual(receipt["status"],"sent")
        self.assertEqual(receipt["result"]["afterTransfer"]["status"],"incomplete")
        self.assertEqual([hook["status"] for hook in receipt["result"]["afterTransfer"]["hooks"]],["succeeded","running"])
        self.assertEqual(self.receipt(self.other)["status"],"ready")
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"],receipt["result"])
        self.assertEqual(len(events.read_text().splitlines()),2)

    def test_destination_attention_skips_after_transfer_and_transfer_retry_does_not_run_it_later(self):
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        self.profile(self.other,installCommand=["sh","-c","exit 7"])
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertNotIn("afterTransfer",result[1]["data"])
        self.assertFalse(events.exists())
        self.profile(self.other,installCommand=["sh","-c","mkdir -p node_modules; touch node_modules/fixture"])
        repaired = self.call(self.other,"setup","--no-herdr",cwd=self.other.checkout)
        self.assertEqual(repaired[0],0,repaired)
        self.assertEqual(self.transfer()[1]["data"],result[1]["data"])
        self.assertFalse(events.exists())

    def test_after_transfer_does_not_run_when_the_worktree_transfer_fails(self):
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        self.corrupt.touch()
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertFalse(events.exists())
        self.assertIsNone(self.receipt(self.fixture)["result"])

    def test_after_transfer_requires_matching_branch_and_safe_destination_path_in_acknowledgement(self):
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        executable = self.fixture.root/"bin/ssh"
        original = executable.read_text()
        for changes in ({"branch":"task/not-the-snapshot"},{"path":"relative/path"},{"path":"/unsafe\npath"}):
            with self.subTest(changes=changes):
                patch = f'''if "_transfer-receive" in command:
    response = json.loads(result.stdout)
    response["data"].update({changes!r})
    result.stdout = json.dumps(response).encode()
sys.stdout.buffer.write(result.stdout)'''
                executable.write_text(original.replace("sys.stdout.buffer.write(result.stdout)",patch))
                result = self.transfer()
                self.assertEqual(result[0],1,result)
                self.assertIn("does not match",result[1]["error"]["message"])
                self.assertFalse(events.exists())
                self.assertEqual(self.receipt(self.fixture)["status"],"sending")
                self.assertEqual(self.receipt(self.other)["status"],"ready")
        executable.write_text(original)
        result = self.transfer()
        self.assertEqual(result[0],0,result)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_adding_a_hook_does_not_change_a_completed_legacy_handoff(self):
        completed = self.transfer()
        self.assertEqual(completed[0],0,completed)
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        result = self.transfer()
        self.assertEqual(result[0],0,result)
        self.assertEqual(result[1]["data"],completed[1]["data"])
        self.assertFalse(events.exists())

    def test_source_configuration_changes_stop_later_follow_up_hooks(self):
        change = '''path=root/"config/graftr/projects.json"
config=json.loads(path.read_text());config["projects"]["bench"]["installOutputs"]=[]
path.write_text(json.dumps(config))'''
        first,events = self.follow_up(name="change",body=change)
        later,_ = self.follow_up(name="later")
        self.profile(hooks={"afterTransfer":[first,later]})
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"]["afterTransfer"]["status"],"incomplete")
        self.assertIn("configuration changed",result[1]["data"]["afterTransfer"]["message"])
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual(self.transfer()[1]["data"],result[1]["data"])

    def test_source_branch_changes_stop_later_follow_up_hooks(self):
        first,events = self.follow_up(name="change",body='import subprocess;subprocess.run(["git","switch","-c","task/changed"],check=True)')
        later,_ = self.follow_up(name="later")
        self.profile(hooks={"afterTransfer":[first,later]})
        result = self.transfer()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"]["afterTransfer"]["status"],"incomplete")
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual(self.receipt(self.other)["status"],"ready")

    def test_unmanaged_sender_supports_after_transfer_without_enrollment_or_setup(self):
        self.assertEqual(self.call(self.fixture,"remove","bench--sample","--yes")[0],0)
        self.fixture.git("worktree","add",str(self.fixture.checkout),"task/sample")
        hook,events = self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        result = self.call(self.fixture,"transfer",".","--to","laptop","--no-herdr",cwd=self.fixture.checkout)
        self.assertEqual(result[0],0,result)
        self.assertTrue(events.exists())
        self.assertFalse((self.fixture.state/"bench--sample.json").exists())
        self.assertFalse((self.fixture.checkout/".env").exists())
        self.assertEqual(self.transfer(self.other,"desktop")[0],0)
        self.assertFalse((self.fixture.state/"bench--sample.json").exists())
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_follow_up_only_retry_skips_successes_keeps_context_and_never_resends_or_reinstalls(self):
        first,events = self.follow_up(name="first")
        failed,_ = self.follow_up(name="board",body='if not (root/"repaired").exists():sys.exit(7)')
        last,_ = self.follow_up(name="last")
        self.profile(hooks={"afterTransfer":[first,failed,last]})
        initial = self.transfer()
        self.assertEqual(initial[0],1,initial)
        old = initial[1]["data"]
        self.assertEqual(old["retryCommand"],"graftr transfer bench--sample --retry-hooks")
        self.assertEqual(old["afterTransfer"]["plannedHooks"],["first","board","last"])
        self.assertEqual(old["afterTransfer"]["attempt"],1)
        ssh_before = self.log.read_bytes()
        before = [(self.tree_git(f,"status","--porcelain=v1","-z"),
                   self.tree_git(f,"rev-parse","HEAD"),
                   (f.checkout/".env").read_bytes(),
                   (f.checkout/"node_modules/install-count").read_bytes()) for f in (self.fixture,self.other)]
        # A follow-up retry has no dependency on the captured Git payload.
        for file in ("packet","work.bundle"):
            (self.generation(self.fixture)/file).unlink()
        (self.fixture.root/"repaired").touch()
        retried = self.retry_follow_ups()
        self.assertEqual(retried[0],0,retried)
        data = retried[1]["data"]
        self.assertTrue(data["followUpOnly"])
        self.assertEqual(data["generation"],old["generation"])
        self.assertEqual(data["afterTransfer"]["attempt"],2)
        self.assertEqual(data["afterTransfer"]["status"],"complete")
        self.assertEqual([hook["status"] for hook in data["afterTransfer"]["hooks"]],["succeeded"]*3)
        self.assertNotIn("retryCommand",data)
        rows=[json.loads(line) for line in events.read_text().splitlines()]
        self.assertEqual([row["env"]["GRAFTR_HOOK_NAME"] for row in rows],["first","board","board","last"])
        self.assertEqual(rows[1]["env"],rows[2]["env"])
        self.assertEqual(self.log.read_bytes(),ssh_before)
        after = [(self.tree_git(f,"status","--porcelain=v1","-z"),
                  self.tree_git(f,"rev-parse","HEAD"),
                  (f.checkout/".env").read_bytes(),
                  (f.checkout/"node_modules/install-count").read_bytes()) for f in (self.fixture,self.other)]
        self.assertEqual(after,before)
        archive=self.fixture.state/f'transfers/bench--sample/follow-up-{old["generation"]}-1.json'
        self.assertEqual(json.loads(archive.read_text()),old)
        self.assertEqual(archive.stat().st_mode & 0o777,0o600)
        self.assertEqual(set(data["timings"]["stagesMs"]),{"run-after-transfer-hook"})
        self.assertEqual(self.retry_follow_ups()[1]["data"],data)
        self.assertEqual(self.transfer()[1]["data"],data)
        self.assertEqual(len(events.read_text().splitlines()),4)
        self.assertEqual(self.log.read_bytes(),ssh_before)

    def test_explicit_follow_up_retry_preserves_each_failed_attempt_until_repaired(self):
        hook,events = self.follow_up(body='if not (root/"repaired").exists():sys.exit(7)')
        self.profile(hooks={"afterTransfer":[hook]})
        first=self.transfer()
        second=self.retry_follow_ups()
        self.assertEqual(first[0],1,first)
        self.assertEqual(second[0],1,second)
        self.assertEqual(second[1]["data"]["afterTransfer"]["attempt"],2)
        self.assertEqual(second[1]["data"]["destinationStatus"],"ready")
        (self.fixture.root/"repaired").touch()
        third=self.retry_follow_ups()
        self.assertEqual(third[0],0,third)
        self.assertEqual(third[1]["data"]["afterTransfer"]["attempt"],3)
        generation=third[1]["data"]["generation"]
        for attempt,result in ((1,first),(2,second)):
            archive=self.fixture.state/f"transfers/bench--sample/follow-up-{generation}-{attempt}.json"
            self.assertEqual(json.loads(archive.read_text()),result[1]["data"])
        self.assertEqual(len(events.read_text().splitlines()),3)

    def test_interrupted_follow_up_can_be_explicitly_resumed_without_replaying_earlier_successes(self):
        first,events=self.follow_up(name="first")
        interrupted,_=self.follow_up(name="board",body='if not (root/"repaired").exists():os.kill(os.getppid(),signal.SIGKILL)')
        last,_=self.follow_up(name="last")
        self.profile(hooks={"afterTransfer":[first,interrupted,last]})
        child=subprocess.run([str(self.native),"transfer","bench--sample","--to","laptop","--no-herdr","--json"],
                             env=self.fixture.env,cwd=self.fixture.repo,capture_output=True,text=True,timeout=30)
        self.assertEqual(child.returncode,-9,child.stdout+child.stderr)
        saved=self.receipt(self.fixture)["result"]
        self.assertEqual(saved["afterTransfer"]["hooks"][-1]["status"],"running")
        (self.fixture.root/"repaired").touch()
        ssh_before=self.log.read_bytes()
        result=self.retry_follow_ups()
        self.assertEqual(result[0],0,result)
        self.assertEqual(self.log.read_bytes(),ssh_before)
        self.assertEqual([json.loads(line)["env"]["GRAFTR_HOOK_NAME"] for line in events.read_text().splitlines()],["first","board","board","last"])
        archive=self.fixture.state/f'transfers/bench--sample/follow-up-{saved["generation"]}-1.json'
        self.assertEqual(json.loads(archive.read_text()),saved)

    def test_explicit_follow_up_retry_allows_fixing_current_command_with_the_same_name(self):
        self.profile(hooks={"afterTransfer":[{"name":"board-handoff","command":["graftr-deliberately-missing-hook"]}]})
        initial=self.transfer()
        self.assertEqual(initial[0],1,initial)
        hook,events=self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        result=self.retry_follow_ups()
        self.assertEqual(result[0],0,result)
        self.assertTrue(events.exists())
        self.assertEqual(result[1]["data"]["generation"],initial[1]["data"]["generation"])

    def test_follow_up_retry_refuses_changed_hook_names_or_order_without_mutating_the_receipt(self):
        first,events=self.follow_up(name="board",body='sys.exit(7)')
        last,_=self.follow_up(name="last")
        self.profile(hooks={"afterTransfer":[first,last]})
        self.assertEqual(self.transfer()[0],1)
        path=self.fixture.state/"transfers/bench--sample/receipt.json"
        saved=path.read_bytes()
        ssh_before=self.log.read_bytes()
        for hooks in ([last,first],[first],[{**first,"name":"changed"},last]):
            with self.subTest(hooks=[hook["name"] for hook in hooks]):
                self.profile(hooks={"afterTransfer":hooks})
                result=self.retry_follow_ups()
                self.assertEqual(result[0],1,result)
                self.assertIn("plan is missing or changed",result[1]["error"]["message"])
                self.assertEqual(path.read_bytes(),saved)
                self.assertEqual(self.log.read_bytes(),ssh_before)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_follow_up_retry_refuses_an_unconfirmed_handoff_without_contacting_destination(self):
        hook,events=self.follow_up()
        self.profile(hooks={"afterTransfer":[hook]})
        self.corrupt.touch()
        self.assertEqual(self.transfer()[0],1)
        saved=self.receipt(self.fixture)
        ssh_before=self.log.read_bytes()
        result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertIn("latest confirmed outgoing",result[1]["error"]["message"])
        self.assertEqual(self.receipt(self.fixture),saved)
        self.assertEqual(self.log.read_bytes(),ssh_before)
        self.assertFalse(events.exists())

    def test_follow_up_retry_does_not_invent_a_plan_for_an_older_receipt(self):
        hook,events=self.follow_up(body='sys.exit(7)')
        self.profile(hooks={"afterTransfer":[hook]})
        self.assertEqual(self.transfer()[0],1)
        path=self.fixture.state/"transfers/bench--sample/receipt.json"
        saved=json.loads(path.read_text())
        saved["result"]["afterTransfer"].pop("plannedHooks")
        saved["result"]["afterTransfer"].pop("attempt")
        path.write_text(json.dumps(saved))
        result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertIn("own CLI",result[1]["error"]["message"])
        self.assertEqual(self.receipt(self.fixture),saved)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_follow_up_retry_refuses_a_superseded_handoff_after_work_has_returned(self):
        hook,events=self.follow_up(body='sys.exit(7)')
        self.profile(hooks={"afterTransfer":[hook]})
        self.assertEqual(self.transfer()[0],1)
        self.assertEqual(self.transfer(self.other,"desktop")[0],0)
        ssh_before=self.log.read_bytes()
        result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertIn("latest confirmed outgoing",result[1]["error"]["message"])
        self.assertEqual(self.log.read_bytes(),ssh_before)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_follow_up_retry_keeps_conflicting_or_symlinked_attempt_evidence_untouched(self):
        hook,events=self.follow_up(body='sys.exit(7)')
        self.profile(hooks={"afterTransfer":[hook]})
        self.assertEqual(self.transfer()[0],1)
        receipt=self.receipt(self.fixture)
        archive=self.fixture.state/f'transfers/bench--sample/follow-up-{receipt["manifest"]["generation"]}-1.json'
        archive.write_text("different evidence");archive.chmod(0o600)
        result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertIn("evidence differs",result[1]["error"]["message"])
        self.assertEqual(archive.read_text(),"different evidence")
        archive.unlink()
        outside=self.fixture.root/"outside-evidence.json"
        outside.write_text("outside");outside.chmod(0o600)
        archive.symlink_to(outside)
        result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertTrue(archive.is_symlink())
        self.assertEqual(outside.read_text(),"outside")
        self.assertEqual(self.receipt(self.fixture),receipt)
        self.assertEqual(len(events.read_text().splitlines()),1)

    def test_follow_up_retry_records_attention_when_source_setup_is_busy_then_can_resume(self):
        hook,events=self.follow_up(body='if not (root/"repaired").exists():sys.exit(7)')
        self.profile(hooks={"afterTransfer":[hook]})
        self.assertEqual(self.transfer()[0],1)
        (self.fixture.root/"repaired").touch()
        key=hashlib.sha256(str(self.fixture.checkout).encode()).hexdigest()
        with (self.fixture.state/f"setup-{key}.lock").open("a") as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            result=self.retry_follow_ups()
        self.assertEqual(result[0],1,result)
        self.assertEqual(result[1]["data"]["afterTransfer"]["status"],"incomplete")
        self.assertEqual(result[1]["data"]["afterTransfer"]["attempt"],2)
        self.assertEqual(len(events.read_text().splitlines()),1)
        self.assertEqual(self.retry_follow_ups()[0],0)
        self.assertEqual(len(events.read_text().splitlines()),2)

    def test_follow_up_retry_options_cannot_accidentally_start_a_new_transfer(self):
        for extra in (("--to","laptop"),("--include","tracked.txt"),("--no-herdr",)):
            with self.subTest(extra=extra):
                result=self.call(self.fixture,"transfer","bench--sample","--retry-hooks",*extra)
                self.assertEqual(result[0],1,result)
                self.assertIn("do not combine",result[1]["error"]["message"])
        result=self.call(self.fixture,"setup","--retry-hooks",cwd=self.fixture.checkout)
        self.assertEqual(result[0],1,result)
        self.assertIn("only valid for transfer",result[1]["error"]["message"])
        self.assertFalse(self.log.exists())
        self.assertFalse(self.other.checkout.exists())

    def test_documented_plannr_command_receives_context_and_retries_without_a_helper_binary(self):
        calls=self.fixture.root/"plannr-calls.jsonl"
        gate=self.fixture.root/"plannr-repaired"
        executable=self.fixture.root/"bin/plannr"
        executable.write_text(f'''#!/usr/bin/python3
import json,os,sys
from pathlib import Path
with Path({str(calls)!r}).open("a") as output:
    output.write(json.dumps({{"cwd":os.getcwd(),"argv":sys.argv[1:]}})+"\\n")
if not Path({str(gate)!r}).exists():
    print("synthetic private plannr failure",file=sys.stderr)
    sys.exit(7)
''')
        executable.chmod(0o700)
        command='exec plannr transfer --to "$GRAFTR_TRANSFER_DESTINATION" --worktree "$GRAFTR_TRANSFER_DESTINATION_PATH" --transfer-id "$GRAFTR_TRANSFER_ID" --lineage "$GRAFTR_TRANSFER_LINEAGE" --if-present --json'
        self.profile(hooks={"afterTransfer":[{"name":"planboard","command":["sh","-c",command],"timeoutMs":600000}]})
        initial=self.transfer()
        self.assertEqual(initial[0],1,initial)
        self.assertTrue(initial[1]["data"]["message"].startswith("Warning:"))
        self.assertEqual(self.receipt(self.other)["status"],"ready")
        self.assertNotIn("synthetic private plannr failure",json.dumps(initial))
        ssh_before=self.log.read_bytes()
        gate.touch()
        retried=self.retry_follow_ups()
        self.assertEqual(retried[0],0,retried)
        expected={"cwd":str(self.fixture.checkout),"argv":[
            "transfer","--to","laptop","--worktree",str(self.other.checkout),
            "--transfer-id",initial[1]["data"]["generation"],
            "--lineage",self.receipt(self.fixture)["manifest"]["lineage"],"--if-present","--json"]}
        self.assertEqual([json.loads(row) for row in calls.read_text().splitlines()],[expected,expected])
        self.assertEqual(self.log.read_bytes(),ssh_before)
        self.assertEqual((self.other.checkout/"node_modules/install-count").read_text(),"1\n")
