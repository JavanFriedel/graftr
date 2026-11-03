import json
from pathlib import Path

from test_cli import NativeCase
import bench


class PluginTests(NativeCase):
    def stub(self, status=0):
        directory = self.fixture.root / "bin"
        directory.mkdir(exist_ok=True)
        executable = directory / "herdr"
        executable.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" >> "$GRAFTR_PLUGIN_CALLS"\nexit {status}\n')
        executable.chmod(0o700)
        return {"PATH": f"{directory}:/usr/bin:/bin", "GRAFTR_PLUGIN_CALLS": str(self.fixture.root / "plugin-calls")}

    def test_install_and_reinstall_embed_the_same_unified_manifest_without_running_hooks(self):
        env = self.stub()
        manifest = self.fixture.root / "data/graftr/plugin/herdr-plugin.toml"
        # Installation writes files; compare stable results and bytes, not read-only snapshots.
        expected = self.invoke(self.reference, ["plugin", "install", "--json"], **env)
        contents = manifest.read_bytes()
        actual = self.invoke([str(self.native)], ["plugin", "install", "--json"], **env)
        self.assertEqual(actual, expected)
        self.assertEqual(manifest.read_bytes(), contents)
        self.assertEqual(manifest.stat().st_mode & 0o777, 0o600)
        self.assertEqual(manifest.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(actual[1]["data"]["hooks"]["startup"], [["graftr", "_startup"]])
        self.assertEqual(actual[1]["data"]["hooks"]["events"], ["worktree.created", "worktree.opened", "workspace.closed", "worktree.removed"])
        self.assertFalse(self.fixture.state.exists())
        calls = (self.fixture.root / "plugin-calls").read_text().splitlines()
        self.assertTrue(all(calls[i:i + 3] == ["plugin", "link", str(manifest.parent)] for i in range(0, len(calls), 3)))

    def test_missing_herdr_and_invalid_arguments_do_not_write_files(self):
        for args in [["plugin", "install", "--json"], ["plugin", "--json"], ["plugin", "install", "extra", "--json"], ["plugin", "install", "--live", "--json"]]:
            self.compare(args, PATH="/no-executables-here")
        self.assertFalse((self.fixture.root / "data/graftr").exists())

    def test_link_failure_retains_manifest_details_and_uncertain_progress(self):
        env = self.stub(7)
        expected = self.invoke(self.reference, ["plugin", "install", "--json"], **env)
        actual = self.invoke([str(self.native)], ["plugin", "install", "--json"], **env)
        self.assertEqual(actual, expected)
        self.assertEqual(actual[0], 1)
        self.assertEqual(actual[1]["outcome"], "partial")
        self.assertEqual(actual[1]["error"]["code"], "PLUGIN_LINK_FAILED")
        self.assertEqual(actual[1]["progress"][-1]["status"], "uncertain")
        self.assertTrue((self.fixture.root / "data/graftr/plugin/herdr-plugin.toml").exists())

    def test_foreign_public_and_symlink_manifests_are_not_overwritten(self):
        env = self.stub()
        self.invoke([str(self.native)], ["plugin", "install", "--json"], **env)
        path = self.fixture.root / "data/graftr/plugin/herdr-plugin.toml"
        original = path.read_bytes()
        for content in [b'id = "foreign"\n', b'# Managed by graftr plugin install. Reinstall to update; do not edit this copy.\nid = "foreign"\n']:
            path.write_bytes(content)
            self.compare(["plugin", "install", "--json"], **env)
            self.assertEqual(path.read_bytes(), content)
        path.write_bytes(original)
        path.chmod(0o644)
        self.compare(["plugin", "install", "--json"], **env)
        path.chmod(0o600)
        saved = path.with_name("saved-manifest")
        path.rename(saved)
        path.symlink_to(saved)
        self.compare(["plugin", "install", "--json"], **env)
        self.assertEqual(saved.read_bytes(), original)

    def test_install_is_independent_of_graftr_registry_and_validates_data_location(self):
        env = self.stub()
        self.fixture.state.mkdir(parents=True, mode=0o700)
        self.fixture.state.with_name("devtree").mkdir(mode=0o700)
        expected = self.invoke(self.reference, ["plugin", "install", "--json"], **env)
        actual = self.invoke([str(self.native)], ["plugin", "install", "--json"], **env)
        self.assertEqual(actual, expected)
        for data_home in ["relative/path", str(self.fixture.root) + "/data/../data"]:
            self.compare(["plugin", "install", "--json"], XDG_DATA_HOME=data_home, **env)
