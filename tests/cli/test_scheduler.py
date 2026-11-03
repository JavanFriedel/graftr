from datetime import datetime
import json
from pathlib import Path
import shutil
import time

from test_cli import NativeCase
from status_fixture import write_record
import bench


class SchedulerTests(NativeCase):
    def fresh(self):
        root = self.fixture.root
        self.assertEqual(root.parent, Path(self.temporary.name))
        shutil.rmtree(root)
        self.fixture = bench.Fixture(root, self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(root / "tmp/transpiler-cache")

    def frozen(self, command):
        self.assertEqual(self.invoke(command, ["create", "bench", "sample", "--no-herdr", "--json"])[0], 0)
        self.assertEqual(self.invoke(command, ["freeze", "bench--sample", "--yes", "--json"])[0], 0)
        self.file = self.fixture.state / "frozen/bench--sample.json"
        record = json.loads(self.file.read_text())
        record["removal"].update(socket=str(self.fixture.root / "unavailable.sock"), workspaceId="old-owned")
        write_record(self.file, record)
        self.config = self.fixture.root / "config/graftr/projects.json"
        config = json.loads(self.config.read_text())
        config["herdr"] = "auto"
        self.config.write_text(json.dumps(config))
        # The fixture's PATH stub records user-systemctl argv; no real manager is contacted.
        executable = self.fixture.root / "bin/systemctl"
        executable.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$HOME/systemctl-calls"\n')
        executable.chmod(0o700)
        return record

    def test_persistent_one_off_schedule_saved_worker_paths_and_cancel_never_stop_service(self):
        outputs = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            self.frozen(command)
            result = self.invoke(command, ["schedule", "bench--sample", "--until", "2100-01-02T12:30:00+02:00", "--json"])
            self.assertEqual(result[0], 0, result)
            record = json.loads(self.file.read_text())
            directory = self.fixture.root / "config/systemd/user"
            timer = next(directory.glob("*.timer"))
            service = next(directory.glob("*.service"))
            self.assertIn("OnCalendar=2100-01-02 10:30:00 UTC", timer.read_text())
            self.assertIn("Persistent=true", timer.read_text())
            self.assertNotIn("OnUnitActiveSec", timer.read_text())
            text = service.read_text()
            self.assertIn('"_thaw" "bench--sample"', text)
            self.assertIn(str(self.config), text)
            self.assertIn(str(self.fixture.state), text)
            self.assertIn("UMask=0077", text)
            if command == [str(self.native)]:
                self.assertIn(f'ExecStart="{self.native}" "_thaw"', text)
                self.assertNotIn("bun", text.split("ExecStart=")[1].splitlines()[0])
            self.assertEqual(timer.stat().st_mode & 0o777, 0o600)
            self.assertEqual(service.stat().st_mode & 0o777, 0o600)
            cancel = self.invoke(command, ["schedule", "bench--sample", "--cancel", "--json"])
            self.assertEqual(cancel[0], 0, cancel)
            self.assertFalse(list(directory.glob("*.timer")))
            self.assertFalse(list(directory.glob("*.service")))
            self.assertNotEqual(json.loads(self.file.read_text())["generation"], record["generation"])
            calls = (self.fixture.root / "home/systemctl-calls").read_text().splitlines()
            self.assertTrue(any("disable --now" in c and c.endswith(".timer") for c in calls))
            self.assertFalse(any(" stop " in c or ("--now" in c and c.endswith(".service")) for c in calls))
            outputs.append(bench.normalize([list(result), list(cancel)], self.fixture.root))
        self.assertEqual(outputs[0], outputs[1])

    def test_unavailable_herdr_retries_after_five_minutes_with_exponential_cap(self):
        outputs = []
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            record = self.frozen(command)
            record.update(until="2026-01-01T00:00:00.000Z", nextAttemptAt=None)
            write_record(self.file, record)
            observations = []
            for attempts, expected in [(0, 300), (1, 600), (10, 21600)]:
                record["attempts"] = attempts
                write_record(self.file, record)
                before = time.time()
                result = self.invoke(command, ["_thaw", "bench--sample", record["generation"], str(self.config), str(self.fixture.state)])
                self.assertEqual(result[0], 0, result)
                response = json.loads(result[1])
                self.assertEqual(response["status"], "deferred")
                saved = json.loads(self.file.read_text())
                retry = datetime.fromisoformat(saved["nextAttemptAt"].replace("Z", "+00:00")).timestamp()
                self.assertGreaterEqual(retry - before, expected - 1)
                self.assertLess(retry - before, expected + 5)
                self.assertEqual(saved["attempts"], attempts + 1)
                self.assertEqual(saved["status"], "frozen")
                observations.append(bench.normalize(response, self.fixture.root))
            self.assertFalse(self.fixture.checkout.exists())
            self.assertFalse((self.fixture.state / "bench--sample.json").exists())
            outputs.append(observations)
        self.assertEqual(outputs[0], outputs[1])

    def test_foreign_timer_is_preserved_and_cancellation_retains_recovery(self):
        for command in [self.reference, [str(self.native)]]:
            self.fresh()
            record = self.frozen(command)
            directory = self.fixture.root / "config/systemd/user"
            directory.mkdir(parents=True, mode=0o700)
            timer = directory / f'graftr-thaw-bench--sample-{record["generation"]}.timer'
            timer.write_text("# Not owned by Graftr\n")
            timer.chmod(0o600)
            result = self.invoke(command, ["schedule", "bench--sample", "--cancel", "--json"])
            self.assertEqual(result[0], 1, result)
            self.assertIn("foreign systemd unit", result[1]["error"]["message"])
            self.assertEqual(timer.read_text(), "# Not owned by Graftr\n")
            # The schedule is fenced first even if deleting a foreign unit is refused.
            self.assertNotEqual(json.loads(self.file.read_text())["generation"], record["generation"])
            self.assertEqual(json.loads(self.file.read_text())["status"], "frozen")
