import json
from pathlib import Path
import shutil
import subprocess

from test_cli import NativeCase
import bench


class CreationTests(NativeCase):
    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def profile(self, **changes):
        path = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(path.read_text())
        config["projects"]["bench"].update(changes)
        path.write_text(json.dumps(config))

    def pair(self, args=(), prepare=None, expected=0):
        results, records, contents = [], [], []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            if prepare:
                prepare()
            result = self.invoke(command, ["create", "bench", "sample", "--no-herdr", *args, "--json"])
            self.assertEqual(result[0], expected, result)
            results.append(bench.normalize(list(result), self.fixture.root))
            state = {}
            if self.fixture.state.exists():
                for file in self.fixture.state.glob("*.json"):
                    state[file.name] = json.loads(file.read_text())
                for file in (self.fixture.state / "setup").glob("*.json"):
                    state["receipt"] = json.loads(file.read_text())
            records.append(bench.normalize(state, self.fixture.root))
            path = result[1]["data"]["path"] if result[1]["data"] else str(self.fixture.checkout)
            contents.append((Path(path, ".env").read_bytes() if Path(path, ".env").exists() else None,
                             Path(path, "node_modules/fixture").read_bytes() if Path(path, "node_modules/fixture").exists() else None))
        self.assertEqual(results[0], results[1])
        self.assertEqual(records[0], records[1])
        self.assertEqual(contents[0], contents[1])
        return results[1]

    def test_create_provisions_environment_dependencies_and_source_readable_records(self):
        self.pair()
        self.fixture.ready(installs=1)
        self.compare(["status", "--json"])
        self.compare(["doctor", "bench--sample", "--json"])
        self.assertFalse(list((self.fixture.state / "creating").glob("*.json")))

    def test_skip_setup_and_skip_install_are_explicit_and_keep_branch(self):
        result = self.pair(["--no-setup"])
        self.assertEqual(result[1]["data"]["status"], "skipped")
        self.assertFalse((self.fixture.checkout / ".env").exists())
        self.assertFalse((self.fixture.state / "bench--sample.json").exists())
        result = self.pair(["--skip-install"])
        self.assertEqual(result[1]["data"]["dependencies"], "skipped")
        self.assertTrue((self.fixture.checkout / ".env").exists())
        self.assertFalse((self.fixture.checkout / "node_modules").exists())

    def test_creation_templates_render_nested_canonical_paths_and_branch_names(self):
        self.pair(prepare=lambda: self.profile(branchName="{{project}}/{{task}}", worktreePath="{{repoName}}/{{branchSlug}}"))
        record = json.loads((self.fixture.state / "bench--sample.json").read_text())
        self.assertEqual(record["branch"], "bench/sample")
        self.assertEqual(record["path"], str(self.fixture.root / "trees/repo/bench-sample"))

    def test_existing_branch_is_reused_only_explicitly_and_never_reset(self):
        def branch():
            self.fixture.git("branch", "feature/retained")
        self.pair(["--branch", "feature/retained"], prepare=branch)
        self.assertEqual(self.fixture.git("rev-parse", "feature/retained").strip(), self.fixture.commit)
        self.pair(["--branch", "not-present"], expected=1)
        self.pair(prepare=lambda: self.fixture.git("branch", "task/sample"), expected=1)

    def test_invalid_explicit_policy_symlinks_and_existing_checkout_are_not_adopted(self):
        for changes in [{"branchName": "{{unknown}}"}, {"worktreePath": "../escape"}, {"branchName": "HEAD"}, {"worktreePath": ".git/{{task}}"}]:
            result = self.pair(prepare=lambda changes=changes: self.profile(**changes), expected=1)
            self.assertEqual(result[1]["error"]["code"], "INVALID_CREATION_CONFIG")
            self.assertFalse(self.fixture.checkout.exists())
        def symlink():
            outside = self.fixture.root / "outside"
            outside.mkdir()
            (self.fixture.root / "trees").symlink_to(outside)
        self.pair(prepare=symlink, expected=1)
        self.pair(prepare=lambda: self.fixture.checkout.mkdir(parents=True), expected=1)

    def test_invalid_setup_profile_keeps_created_plain_checkout_and_truthful_receipt(self):
        result = self.pair(prepare=lambda: self.profile(localPorts=[0, 1]), expected=1)
        self.assertEqual(result[1]["error"]["code"], "SETUP_UNMANAGED")
        self.assertTrue(self.fixture.checkout.is_dir())
        self.assertFalse((self.fixture.checkout / ".env").exists())
        self.assertEqual(self.fixture.git("rev-parse", "task/sample").strip(), self.fixture.commit)

    def test_open_and_repeated_create_reuse_prepared_checkout_without_reinstall(self):
        self.pair()
        for args in [["open", "bench--sample", "--no-herdr", "--json"], ["create", "bench", "sample", "--no-herdr", "--json"]]:
            expected = self.invoke(self.reference, args)
            actual = self.invoke([str(self.native)], args)
            self.assertEqual(bench.normalize(list(actual), self.fixture.root), bench.normalize(list(expected), self.fixture.root))
            self.fixture.ready(installs=1)
        for columns in [24, 40, 80]:
            expected = self.invoke(self.reference, ["open", "bench--sample", "--no-herdr"], COLUMNS=str(columns))
            actual = self.invoke([str(self.native)], ["open", "bench--sample", "--no-herdr"], COLUMNS=str(columns))
            self.assertEqual(actual, expected)

    def test_named_port_allocation_preserves_configuration_order_in_overlapping_ranges(self):
        self.pair(prepare=lambda: self.profile(ports={"z-first": [61000, 61002], "a-second": [61000, 61002]}))
        task = json.loads((self.fixture.state / "bench--sample.json").read_text())
        self.assertEqual(task["ports"], {"z-first": 61000, "a-second": 61001})

    def test_native_create_requires_no_bun_or_python_runtime(self):
        result = self.invoke([str(self.native)], ["create", "bench", "sample", "--no-herdr", "--json"], PATH="/usr/bin:/bin")
        self.assertEqual(result[0], 0)
        self.fixture.ready(installs=1)
