import json
import os
from pathlib import Path
import pty
import select
import shutil
import subprocess
import time

from test_cli import NativeCase
import bench


class NukeTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        (self.fixture.state / ".auto-prune").touch(mode=0o600)
        return self.fixture.checkout

    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def terminal(self, command, branch="task/sample", approve=True, change=None):
        master, slave = pty.openpty()
        child = subprocess.Popen([*command, "nuke", "bench--sample", "--json"], cwd=self.fixture.repo, env=self.fixture.env,
                                 stdin=slave, stderr=slave, stdout=subprocess.PIPE, text=True)
        os.close(slave)
        prompt = bytearray()
        def read_until(text):
            deadline = time.monotonic() + 5
            while text not in prompt:
                remaining = deadline - time.monotonic()
                self.assertGreater(remaining, 0)
                self.assertTrue(select.select([master], [], [], remaining)[0])
                prompt.extend(os.read(master, 4096))
        try:
            read_until(b"Type the exact branch name")
            if change:
                change()
            os.write(master, (branch + "\n").encode())
            if branch == "task/sample":
                read_until(b"Delete this worktree and branch? [y/N]")
                os.write(master, b"yes\n" if approve else b"n\n")
            stdout, _ = child.communicate(timeout=10)
            return (child.returncode, json.loads(stdout), ""), bytes(prompt)
        finally:
            os.close(master)
            if child.poll() is None:
                child.kill()
                child.wait()

    def test_nuke_requires_explicit_target_terminal_confirmation_and_no_bypass(self):
        self.managed()
        for args in [["nuke", "--json"], ["nuke", "@", "--json"], ["nuke", "bench--sample", "--yes", "--json"],
                     ["nuke", "bench--sample", "--force", "--json"], ["nuke", "bench--sample", "--json"]]:
            result = self.compare(args)
            self.assertEqual(result[0], 1)
        self.assertTrue(self.fixture.checkout.is_dir())

    def test_typed_ancestry_merged_deletion_matches_source_and_keeps_main(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            path = self.managed()
            result, _ = self.terminal(command)
            self.assertEqual(result[0], 0)
            self.assertEqual(result[1]["data"]["status"], "nuked")
            self.assertFalse(path.exists())
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            self.assertEqual(self.fixture.git("rev-parse", "main").strip(), self.fixture.commit)
            self.assertEqual(subprocess.run(["git", "-C", str(self.fixture.repo), "show-ref", "--verify", "--quiet", "refs/heads/task/sample"], env=self.fixture.env).returncode, 1)
            results.append(bench.normalize(list(result), self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_squash_merged_content_is_proven_before_exact_ref_deletion(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            path = self.managed()
            (path / "feature.txt").write_text("merged content\n")
            self.fixture.git("-C", str(path), "add", "feature.txt")
            self.fixture.git("-C", str(path), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "-qm", "feature")
            self.fixture.git("merge", "--squash", "task/sample")
            self.fixture.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "-qm", "squashed")
            result, prompt = self.terminal(command)
            self.assertEqual(result[0], 0)
            self.assertIn(b"Merged by content", prompt)
            self.assertEqual((self.fixture.repo / "feature.txt").read_text(), "merged content\n")
            results.append(bench.normalize(list(result), self.fixture.root))
        self.assertEqual(results[0], results[1])

    def test_wrong_branch_or_declined_second_confirmation_preserves_everything(self):
        path = self.managed()
        for command in [self.reference, [str(self.native)]]:
            for branch, approve in [("wrong-branch", True), ("task/sample", False)]:
                result, _ = self.terminal(command, branch, approve)
                self.assertEqual(result[0], 0)
                self.assertEqual(result[1]["outcome"], "cancelled")
                self.assertTrue(path.is_dir())
                self.assertTrue((self.fixture.state / "bench--sample.json").exists())

    def test_unmerged_commits_are_kept(self):
        path = self.managed()
        (path / "feature.txt").write_text("not merged\n")
        self.fixture.git("-C", str(path), "add", "feature.txt")
        self.fixture.git("-C", str(path), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit", "-qm", "feature")
        result = self.compare(["nuke", "bench--sample", "--json"])
        self.assertEqual(result[1]["error"]["phase"], "preflight")
        self.assertIn("unmerged commits", result[1]["error"]["message"])
        self.assertTrue(path.is_dir())

    def test_branch_tip_moving_during_confirmation_refuses_deletion(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            path = self.managed()
            tree = self.fixture.git("rev-parse", "main^{tree}").strip()
            commit = self.fixture.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test", "commit-tree", tree,
                                      "-p", self.fixture.commit, "-m", "moved during prompt").strip()
            def change():
                self.fixture.git("update-ref", "refs/heads/task/sample", commit, self.fixture.commit)
            result, _ = self.terminal(command, change=change)
            self.assertEqual(result[0], 1, result)
            self.assertTrue(path.is_dir())
            self.assertTrue((self.fixture.state / "bench--sample.json").exists())
            self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), commit)
            results.append(bench.normalize(list(result), self.fixture.root))
        self.assertEqual(results[0], results[1])
