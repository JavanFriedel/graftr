import json
from pathlib import Path
import shutil

from test_cli import NativeCase
from status_fixture import write_record
import bench


class MigrationTests(NativeCase):
    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def legacy(self, route="active"):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        self.config = self.fixture.root / "config/graftr/projects.json"
        profile = json.loads(self.config.read_text())
        project = profile["projects"]["bench"]
        project.update(httpsPorts=[45000, 46000], originsKey="ALLOWED_DEV_ORIGINS")
        project.pop("envOverrides", None)
        self.config.write_text(json.dumps(profile))
        path = self.fixture.state / "bench--sample.json"
        task = json.loads(path.read_text())
        task.update(schemaVersion=2, route=route, host="fixture.tailnet.test", httpsPort=45000,
                    url="https://fixture.tailnet.test:45000")
        task["overrides"] = {"PORT": "40000", "BASE_URL": task["url"], "AUTH_URL": task["url"],
                             "ALLOWED_DEV_ORIGINS": "http://localhost:40000," + task["url"]}
        task.pop("envOverrides", None)
        write_record(path, task)
        return path

    def test_current_configuration_is_unchanged_and_preview_has_no_writes(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        for args in [["migrate", "--json"], ["migrate"], ["migrate", "extra", "--json"]]:
            self.compare(args)

    def test_legacy_preview_converts_routes_without_querying_or_mutating_services(self):
        self.legacy()
        result = self.compare(["migrate", "--json"])
        self.assertEqual(result[1]["data"]["tasks"][0]["route"], "active")
        self.assertEqual(len(result[1]["data"]["projects"]), 1)
        self.assertFalse((self.fixture.root / "home/forbidden-service").exists())

    def test_apply_keeps_private_backups_and_owned_route_receipts_without_serve_mutation(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            task_path = self.legacy()
            previous = task_path.read_bytes()
            original_config = self.config.read_bytes()
            result = self.invoke(command, ["migrate", "--apply", "--json"])
            self.assertEqual(result[0], 0, result)
            backup = Path(result[1]["data"]["backupPath"])
            self.assertEqual((backup / task_path.name).read_bytes(), previous)
            self.assertEqual((backup / "projects.json").read_bytes(), original_config)
            self.assertEqual(backup.stat().st_mode & 0o777, 0o700)
            self.assertEqual((backup / task_path.name).stat().st_mode & 0o777, 0o600)
            task = json.loads(task_path.read_text())
            self.assertEqual(task["schemaVersion"], 4)
            self.assertEqual(task["cleanup"]["status"], "armed")
            self.assertEqual(task["cleanup"]["attempt"], 0)
            receipts = list((self.fixture.state / "tailscale-hooks").rglob("*.json"))
            self.assertEqual(len(receipts), 1)
            route = json.loads(receipts[0].read_text())
            self.assertFalse((self.fixture.root / "home/forbidden-service").exists())
            result[1]["data"]["backupPath"] = "<private-migration-backup>"
            results.append(bench.normalize([list(result), task, route, json.loads(self.config.read_text())], self.fixture.root))
            self.assertEqual(self.invoke(command, ["migrate", "--apply", "--json"])[1]["data"]["status"], "unchanged")
        self.assertEqual(results[0], results[1])

    def test_blocked_conversion_never_partially_rewrites_configuration_or_records(self):
        task_path = self.legacy()
        task = json.loads(task_path.read_text())
        task["route"] = "foreign"
        write_record(task_path, task)
        self.compare(["migrate", "--json"])
        config, record = self.config.read_bytes(), task_path.read_bytes()
        results = [self.invoke(command, ["migrate", "--apply", "--json"]) for command in [self.reference, [str(self.native)]]]
        self.assertEqual(results[0], results[1])
        self.assertEqual(results[0][0], 1)
        self.assertEqual(self.config.read_bytes(), config)
        self.assertEqual(task_path.read_bytes(), record)
        self.assertFalse((self.fixture.state / "migration-backups").exists())

    def test_current_cleanup_and_profile_commands_are_rewritten_without_replaying_hooks(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        config = self.fixture.root / "config/graftr/projects.json"
        value = json.loads(config.read_text())
        value["projects"]["bench"]["setupHooks"] = {"cleanup": [{"name": "old-route", "command": ["bun", "-e", "import tailscaleHook"]}]}
        config.write_text(json.dumps(value))
        self.compare(["migrate", "--json"])

    def test_legacy_directories_move_last_and_retain_aliases_and_lock_inode(self):
        results = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
            config_dir = self.fixture.root / "config/graftr"
            config_dir.rename(config_dir.with_name("devtree"))
            state = self.fixture.state
            state.rename(state.with_name("devtree"))
            lock = state.with_name("devtree") / "operation.lock"
            # Retain the existing lock inode across the directory move.
            inode = lock.stat().st_ino
            result = self.invoke(command, ["migrate", "--apply", "--json"])
            self.assertEqual(result[0], 0, result)
            self.assertTrue(config_dir.with_name("devtree").is_symlink())
            self.assertTrue(state.with_name("devtree").is_symlink())
            self.assertEqual((state / "operation.lock").stat().st_ino, inode)
            results.append(bench.normalize(list(result), self.fixture.root))
        self.assertEqual(results[0], results[1])
