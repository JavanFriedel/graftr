import hashlib
import json
from pathlib import Path

from test_cli import NativeCase
from status_fixture import write_record, plan


class PostgresTests(NativeCase):
    def managed(self):
        self.fixture.invoke(["create", "bench", "sample", "--no-herdr", "--json"])
        record = self.fixture.state / "bench--sample.json"
        task = json.loads(record.read_text())
        task["retire"] = plan("retire")
        task["retire"]["hooks"][0]["command"] = ["graftr", "hook", "postgres"]
        write_record(record, task)
        self.target = Path(task["path"]) / ".env"
        self.original = self.target.read_bytes()
        baseline = self.fixture.repo / ".env"
        baseline.write_text("DATABASE_URL=postgresql://devuser:synthetic%3Apassword@127.0.0.1:5544/development?sslmode=disable\n")
        baseline.chmod(0o600)
        self.mock = self.fixture.root / "tmp/postgres-state.json"
        directory = self.fixture.root / "bin"
        script = directory / "psql"
        script.write_text('''#!/usr/bin/python3
import fcntl, json, os, sys
from pathlib import Path
args = sys.argv[1:]
exe = Path(sys.argv[0]).name
data = sys.stdin.read() if exe == "psql" else ""
with open(os.environ["MOCK_POSTGRES"], "r+") as stream:
    fcntl.flock(stream, fcntl.LOCK_EX)
    state = json.load(stream)
    name = next((arg[5:] for arg in args if arg.startswith("name=")), None)
    call = {"exe": exe, "args": args, "input": data, "pgEnv": {key: value for key, value in os.environ.items() if key.startswith("PG")}}
    state.setdefault("calls", []).append(call)
    code = 0
    if exe == "pg_dump":
        print("-- synthetic streaming dump")
        if state.get("dump_failure"):
            code = 7
    elif data.startswith("SELECT 1"):
        if name in state.get("databases", []):
            print("1")
    elif data.startswith("CREATE DATABASE"):
        state.setdefault("databases", []).append(name)
        if state.get("create_lost_reply"):
            code = 7
    elif data.startswith("DROP DATABASE"):
        if state.get("busy"):
            print("synthetic-private-error", file=sys.stderr)
            code = 7
        else:
            state["databases"].remove(name)
    else:
        if state.get("restore_failure"):
            code = 8
    stream.seek(0)
    json.dump(state, stream)
    stream.truncate()
sys.exit(code)
''')
        script.chmod(0o700)
        (directory / "pg_dump").symlink_to("psql")
        self.env = {"PATH": f"{directory}:/usr/bin:/bin", "MOCK_POSTGRES": str(self.mock),
                    "GRAFTR_TASK_ID": task["id"], "GRAFTR_WORKTREE_PATH": task["path"],
                    "GRAFTR_RETIRE_RUN_ID": task["retire"]["runId"], "PGHOST": "forbidden-inherited", "PGSERVICE": "forbidden-service",
                    "PGPASSWORD": "wrong-inherited"}
        self.task = task
        return task

    def receipt_path(self):
        key = hashlib.sha256(json.dumps([self.task["id"], self.task["retire"]["runId"]], separators=(",", ":")).encode()).hexdigest()
        return self.fixture.state / "postgres-hooks" / key / (self.task["id"] + ".json")

    def pair(self, phase, owned=None, **state):
        results, records, states, targets = [], [], [], []
        for command in [self.reference, [str(self.native)]]:
            self.target.write_bytes(self.original)
            receipt = self.receipt_path()
            receipt.unlink(missing_ok=True)
            if owned:
                write_record(receipt, owned)
                receipt.parent.parent.chmod(0o700)
            self.mock.write_text(json.dumps({"databases": ["development"], **state}))
            results.append(self.invoke(command, ["hook", "postgres"], GRAFTR_HOOK_PHASE=phase, **self.env))
            records.append(json.loads(receipt.read_text()) if receipt.exists() else None)
            observed = json.loads(self.mock.read_text())
            observed["calls"] = sorted(observed.get("calls", []), key=lambda item: json.dumps(item, sort_keys=True))
            states.append(observed)
            targets.append(self.target.read_bytes())
        self.assertEqual(results[0], results[1])
        self.assertEqual(records[0], records[1])
        self.assertEqual(states[0], states[1])
        self.assertEqual(targets[0], targets[1])
        for call in states[1]["calls"]:
            self.assertEqual(call["pgEnv"], {"PGPASSWORD": "synthetic:password"})
            self.assertNotIn("synthetic", str(call["args"]))
            self.assertNotIn("--force", call["args"])
        self.assertNotIn("synthetic:password", str(results))
        self.assertNotIn("synthetic%3Apassword", str(records))
        return results[1], records[1], states[1]

    def test_create_copy_private_environment_and_idempotent_reuse(self):
        self.managed()
        result, owned, state = self.pair("afterEnvironment")
        self.assertEqual(result, (0, "", ""))
        self.assertEqual(owned["state"], "copied")
        self.assertEqual(owned["database"], "development_bench_sample")
        self.assertIn(owned["database"], state["databases"])
        self.assertTrue(self.target.read_bytes().startswith(self.original))
        self.assertIn(b"synthetic%3Apassword", self.target.read_bytes())
        self.assertEqual(self.target.stat().st_mode & 0o777, 0o600)
        self.original = self.target.read_bytes()
        result, after, state = self.pair("afterEnvironment", owned, databases=["development", owned["database"]])
        self.assertEqual(result[0], 0)
        self.assertEqual(after, owned)
        self.assertEqual(state["calls"], [])

    def test_refuse_foreign_database_and_recover_only_recorded_partial_copy(self):
        self.managed()
        result, owned, _ = self.pair("afterEnvironment", databases=["development", "development_bench_sample"])
        self.assertEqual(result[0], 1)
        self.assertIsNone(owned)
        result, owned, _ = self.pair("afterEnvironment", restore_failure=True)
        self.assertEqual(result[0], 1)
        self.assertEqual(owned["state"], "created")
        result, after, state = self.pair("afterEnvironment", owned, databases=["development", owned["database"]])
        self.assertEqual(result[0], 0)
        self.assertEqual(after["state"], "copied")
        self.assertTrue(any(call["input"].startswith("DROP DATABASE") for call in state["calls"]))

    def test_lost_create_reply_retains_intent_and_no_secret_is_logged(self):
        self.managed()
        result, owned, _ = self.pair("afterEnvironment", create_lost_reply=True)
        self.assertEqual(result[0], 1)
        self.assertEqual(owned["state"], "creating")
        self.assertNotIn("synthetic-private-error", str(result))

    def test_retire_is_no_force_and_preserves_busy_database_and_receipt(self):
        self.managed()
        _, owned, _ = self.pair("afterEnvironment")
        result, after, state = self.pair("retire", owned, databases=["development", owned["database"]], busy=True)
        self.assertEqual(result[0], 1)
        self.assertEqual(after, owned)
        self.assertIn(owned["database"], state["databases"])
        result, after, state = self.pair("retire", owned, databases=["development", owned["database"]])
        self.assertEqual(result[0], 0)
        self.assertEqual(after["state"], "dropped")
        self.assertEqual(state["databases"], ["development"])
        result, after, state = self.pair("retire")
        self.assertEqual(result[0], 0)
        self.assertIsNone(after)
        self.assertEqual(state["calls"], [])

    def test_changed_baseline_identity_and_stale_context_do_not_touch_database(self):
        self.managed()
        _, owned, _ = self.pair("afterEnvironment")
        baseline = self.fixture.repo / ".env"
        baseline.write_text("DATABASE_URL=postgres://different@127.0.0.1/development\n")
        # Error parity is checked separately here because the inherited password is
        # deliberately absent from this new approved baseline.
        for command in [self.reference, [str(self.native)]]:
            self.mock.write_text(json.dumps({"databases": ["development"]}))
            write_record(self.receipt_path(), owned)
            result = self.invoke(command, ["hook", "postgres"], GRAFTR_HOOK_PHASE="retire", **self.env)
            self.assertEqual(result[0], 1)
            self.assertEqual(json.loads(self.mock.read_text()).get("calls", []), [])
        self.compare(["hook", "postgres"], GRAFTR_HOOK_PHASE="retire", **{**self.env, "GRAFTR_RETIRE_RUN_ID": "stale"})
