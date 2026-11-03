import copy
import json
from pathlib import Path

from test_cli import NativeCase
from status_fixture import write_record, STAMP


class InspectionTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        path = self.fixture.state / "bench--sample.json"
        return json.loads(path.read_text()), path

    def completion(self, words, cwd=None):
        return self.compare(["_complete", "--", *words], cwd=cwd)

    def other(self, task, path=None, branch=None, project="other", task_name="sample"):
        record = copy.deepcopy(task)
        record.update(id=f"{project}--{task_name}", project=project,
                      path=str(path or self.fixture.root / f"{project}/sample"),
                      repo=str(self.fixture.root / "other-repo") if project == "other" else task["repo"],
                      branch=branch or task["branch"])
        record["localPort"] += 1
        record["overrides"]["PORT"] = str(record["localPort"])
        write_record(self.fixture.state / (record["id"] + ".json"), record)
        return record

    def test_static_flags_projects_hooks_and_repeatable_values(self):
        config = json.loads((self.fixture.root / "config/graftr/projects.json").read_text())
        config["projects"]["bench"]["hooks"] = {"afterDependencies": [{"name": "custom-check", "command": ["false"]}]}
        (self.fixture.root / "config/graftr/projects.json").write_text(json.dumps(config))
        words = [[""], ["cl"], ["help", "hoo"], ["help", "open", ""], ["completion", ""], ["completion", "bash", ""],
                 ["open", "--"], ["open", "--wait", "--"], ["doctor", "--"], ["plugin", ""], ["plugin", "install", ""],
                 ["create", "be"], ["open", "--project", "=", "b"], ["open", "--project=b"], ["list", "bench", ""],
                 ["env", ""], ["env", "refresh", "--take-baseline", "PORT", "--"], ["hook", ""], ["hook", "tailscale", ""],
                 ["adopt", "--"], ["adopt", "--hook", "after"], ["adopt", "--hook=after"],
                 ["adopt", "--hook", "afterDependencies/custom-check", "--"], ["unknown", "--"], ["thaw", "--note", ""]]
        for value in words:
            self.completion(value)
        self.assertFalse(self.fixture.state.exists())

    def test_targets_and_completion_infer_scope_and_omit_ambiguous_aliases(self):
        task, _ = self.managed()
        self.other(task)
        for cwd in [self.fixture.root, self.fixture.repo, Path(task["path"])]:
            for words in [["open", ""], ["nuke", ""], ["open", "sam"], ["open", "--project=bench", ""], ["logs", "other--"], ["open", "bench--sample", ""]]:
                self.completion(words, cwd=cwd)
        for selector in ["sample", "task/sample", task["id"], task["path"], "../trees/sample"]:
            self.compare(["logs", selector, "--json"], cwd=self.fixture.repo)
        self.compare(["logs", "sample", "--project", "other", "--json"], cwd=self.fixture.root)
        self.compare(["logs", task["id"], "--project", "other", "--json"])

    def test_paths_nested_directories_symlinks_files_and_missing_checkouts(self):
        task, _ = self.managed()
        checkout = Path(task["path"])
        (checkout / "nested").mkdir()
        link = self.fixture.root / "link"
        link.symlink_to(checkout)
        for selector in ["./link/nested", "./trees/sample", "trees/sample/", "./trees/sample/nested"]:
            self.compare(["logs", selector, "--json"], cwd=self.fixture.root)
        self.compare(["logs", "--json"], cwd=checkout / "nested")
        self.compare(["logs", "@", "--json"], cwd=checkout)
        self.compare(["logs", "./trees/sample/tracked.txt", "--json"], cwd=self.fixture.root)
        self.compare(["logs", "./trees/sample/missing", "--json"], cwd=self.fixture.root)
        self.fixture.git("worktree", "remove", str(checkout))
        self.compare(["logs", str(checkout), "--json"])
        self.compare(["logs", str(checkout / "missing"), "--json"])

    def test_existing_id_bypasses_unrelated_invalid_records(self):
        task, _ = self.managed()
        write_record(self.fixture.state / "broken.json", {"private": "synthetic-secret"})
        self.compare(["logs", task["id"], "--json"])

    def test_refs_exclude_checked_out_and_frozen_branches_without_fetching(self):
        self.managed()
        for name in ["feature/free", "feature/used", "feature/frozen"]:
            self.fixture.git("branch", name)
        linked = self.fixture.root / "unmanaged"
        self.fixture.git("worktree", "add", "-q", str(linked), "feature/used")
        from status_fixture import seed_status
        files = seed_status(self.fixture, 1)
        frozen = json.loads(files["frozen"][0].read_text())
        frozen["removal"]["currentBranch"] = "feature/frozen"
        write_record(files["frozen"][0], frozen)
        for words in [["create", "bench", "new", "--base", ""], ["create", "bench", "new", "--branch", ""],
                      ["create", "bench", "new", "--base=fea"], ["adopt", ""], ["thaw", ""], ["schedule", "--project=bench", ""]]:
            self.completion(words)

    def test_control_characters_filtered_and_shell_metacharacters_stay_literal(self):
        task, path = self.managed()
        task["path"] = str(self.fixture.root / "checkout space $(touch PWN)")
        write_record(path, task)
        self.completion(["open", "checkout"], cwd=self.fixture.root)
        task["path"] = str(self.fixture.root / "bad\nname")
        write_record(path, task)
        self.completion(["open", ""], cwd=self.fixture.root)
        self.assertFalse((self.fixture.root / "PWN").exists())

    def test_completion_is_quiet_with_missing_invalid_and_split_state(self):
        self.completion(["open", ""])
        write_record(self.fixture.state / "broken.json", {"private": "synthetic-secret"})
        self.completion(["open", ""])
        self.fixture.state.with_name("devtree").mkdir()
        result = self.completion(["open", ""])
        self.assertEqual(result, (0, "", ""))

    def test_inventory_reports_registered_unmanaged_stale_and_conflicting_records(self):
        task, _ = self.managed()
        unmanaged = self.fixture.root / "trees/unmanaged space"
        self.fixture.git("worktree", "add", "-qb", "feature/unmanaged", str(unmanaged))
        self.fixture.git("worktree", "lock", "--reason", "manual lock", str(unmanaged))
        self.other(task, path=Path(task["path"]), project="bench", task_name="duplicate")
        self.compare(["list", "bench", "--json"])
        for columns in [24, 60, 120]:
            self.compare(["list", "bench"], COLUMNS=str(columns))
        self.fixture.git("worktree", "unlock", str(unmanaged))
        self.fixture.git("-C", str(unmanaged), "checkout", "--detach", "-q")
        self.compare(["list", "bench", "--json"])

    def test_inventory_errors_hide_subprocess_and_configuration_contents(self):
        config = self.fixture.root / "config/graftr/projects.json"
        config.write_text('{"password":"synthetic-secret" INVALID}')
        result = self.compare(["list", "bench", "--json"])
        self.assertNotIn("synthetic-secret", json.dumps(result))
        config.write_text(json.dumps({"schemaVersion": 2, "projects": {"bench": {"repo": str(self.fixture.root / "missing"), "worktreeRoot": str(self.fixture.root / "trees")}}}))
        self.compare(["list", "bench", "--json"])
        self.compare(["list", "missing", "--json"])

    def test_logs_metadata_does_not_read_contents_and_latest_is_raw_private_output(self):
        task, _ = self.managed()
        folder = self.fixture.state / "logs" / task["id"]
        log = folder / "21000101T000000000Z-custom-abcdef.log"
        log.write_bytes(b"synthetic-private-log\nwithout-final-newline")
        log.chmod(0o600)
        self.compare(["logs", task["id"], "--json"])
        self.compare(["logs", task["id"]], COLUMNS="40")
        result = self.compare(["logs", task["id"], "--latest"])
        self.assertEqual(result[1], "synthetic-private-log\nwithout-final-newline")
        self.compare(["logs", task["id"], "--latest", "--json"])
        log.chmod(0o644)
        self.compare(["logs", task["id"], "--json"])
        result = self.compare(["logs", task["id"], "--latest"])
        self.assertNotIn("synthetic-private-log", str(result))

    def test_adopted_logs_use_external_metadata_without_setup_or_herdr(self):
        path = self.fixture.root / "external"
        path.mkdir()
        record = {"schemaVersion": 1, "id": "bench--external", "project": "bench", "repo": str(self.fixture.repo), "path": str(path),
                  "branch": "feature/external", "adoptedAt": STAMP, "socket": None, "workspaceId": None,
                  "localPort": None, "url": None, "environment": "external", "dependencies": "external", "hooks": []}
        write_record(self.fixture.state / "adopted/bench--external.json", record)
        for args in [["logs", record["id"], "--json"], ["logs", record["id"], "--latest", "--json"], ["logs", record["id"], "--latest"]]:
            self.compare(args)
        self.compare(["logs", "--json"], cwd=path)

    def test_moved_caller_pane_uses_explicit_socket_and_pane_not_focused_workspace(self):
        task, record_path = self.managed()
        socket_path, calls, errors = self.serve(lambda request: {"id": request["id"], "result": {"pane": {"workspace_id": "owned-workspace"}}})
        task.update(socket=socket_path, workspaceId="owned-workspace")
        write_record(record_path, task)
        self.compare(["logs", "--json"], cwd=self.fixture.root, HERDR_ENV="1", HERDR_SOCKET_PATH=socket_path,
                    HERDR_WORKSPACE_ID="stale", HERDR_PANE_ID="explicit-caller-pane")
        self.assertEqual([call["params"] for call in calls], [{"caller_pane_id": "explicit-caller-pane"}] * 2)
        self.assertFalse(errors)

    def test_rpc_identity_mismatch_is_refused_without_leaking_response(self):
        task, record_path = self.managed()
        socket_path, _, errors = self.serve(lambda request: {"id": "wrong", "result": {"secret": "synthetic-secret"}})
        task.update(socket=socket_path, workspaceId="owned-workspace")
        write_record(record_path, task)
        result = self.compare(["logs", "--json"], cwd=self.fixture.root, HERDR_ENV="1", HERDR_SOCKET_PATH=socket_path,
                             HERDR_WORKSPACE_ID="owned-workspace", HERDR_PANE_ID="explicit-caller-pane")
        self.assertNotIn("synthetic-secret", json.dumps(result))
        self.assertFalse(errors)

    def test_xdg_state_does_not_require_home_to_be_nonempty(self):
        self.compare(["status", "--json"], HOME="")

    def test_git_backed_inspection_needs_git_but_no_bun_on_path(self):
        task, _ = self.managed()
        for args in [["list", "bench", "--json"], ["_complete", "--", "open", ""], ["logs", task["id"], "--json"]]:
            self.compare(args, PATH="/usr/bin:/bin")

    def test_completion_uses_utf16_sort_order_for_non_bmp_aliases(self):
        task, _ = self.managed()
        for suffix, name in [("astral", "order-𐀀"), ("bmp", "order-\ue000")]:
            self.other(task, path=self.fixture.root / name, project="bench", task_name=suffix)
        result = self.completion(["open", "order-"], cwd=self.fixture.root)
        self.assertEqual(result[1].splitlines(), ["order-𐀀", "order-\ue000"])

    def test_plugin_actions_use_selected_workspace_and_do_not_query_caller_pane(self):
        task, path = self.managed()
        selected = self.other(task, project="bench", task_name="selected")
        for record in (task, selected):
            record.update(socket=str(self.fixture.root / "not-a-live-server.sock"), workspaceId="selected" if record["id"] == selected["id"] else "worker")
            write_record(self.fixture.state / (record["id"] + ".json"), record)
        env = {"HERDR_ENV": "1", "HERDR_SOCKET_PATH": task["socket"], "HERDR_WORKSPACE_ID": "selected",
               "HERDR_PANE_ID": "must-not-be-queried", "HERDR_PLUGIN_ACTION_ID": "open", "HERDR_PLUGIN_ROOT": task["path"]}
        for args in [["logs", "--json"], ["logs", "@", "--json"], ["logs", task["id"], "--json"]]:
            self.compare(args, cwd=Path(task["path"]), **env)
