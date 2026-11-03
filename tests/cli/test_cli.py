import json
import gzip
import os
import errno
import pty
import select
import time
import fcntl
import struct
import termios
import socket
import threading
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

PROJECT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(PROJECT / "benchmarks"))
import bench


class NativeCase(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.native = Path(os.environ.get("GRAFTR_NATIVE", PROJECT / "dist/graftr")).resolve()
        if not cls.native.is_file():
            raise RuntimeError("Build the executable first (./build.sh), or set GRAFTR_NATIVE")
        # Native mode checks behavior and repeatability; an explicit independent CLI
        # enables cross-implementation/version comparisons without bundling TypeScript.
        cls.reference = json.loads(os.environ["GRAFTR_REFERENCE"]) if "GRAFTR_REFERENCE" in os.environ else [str(cls.native)]
        if not isinstance(cls.reference, list) or not cls.reference or any(not isinstance(arg, str) or not arg for arg in cls.reference):
            raise RuntimeError("GRAFTR_REFERENCE must be a nonempty JSON argv array")

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="graftr-native-parity-", dir=tempfile.gettempdir())
        self.addCleanup(self.temporary.cleanup)
        self.fixture = bench.Fixture(Path(self.temporary.name) / "fixture", self.reference, 10)
        self.fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(self.fixture.root / "tmp/transpiler-cache")

    def invoke(self, command, args, cwd=None, **env):
        child = subprocess.run([*command, *args], env={**self.fixture.env, **env}, cwd=cwd or self.fixture.repo,
                               stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
        stdout = child.stdout
        if "--json" in args and child.stdout.startswith("{"):
            stdout = json.loads(child.stdout)
        return child.returncode, stdout, child.stderr

    def compare(self, args, cwd=None, **env):
        before = bench.readonly_snapshot(self.fixture)
        expected = self.invoke(self.reference, args, cwd=cwd, **env)
        actual = self.invoke([str(self.native)], args, cwd=cwd, **env)
        self.assertEqual(actual, expected, f"{args}, {env}")
        self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        return actual

    def serve(self, respond, count=2):
        self.socket_count = getattr(self, "socket_count", 0) + 1
        path = self.fixture.root / f"herdr-{self.socket_count}.sock"
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        server.bind(str(path))
        server.listen()
        server.settimeout(5)
        calls, errors = [], []

        def work():
            try:
                for _ in range(count):
                    connection, _ = server.accept()
                    with connection:
                        request = json.loads(connection.makefile("rb").readline())
                        calls.append(request)
                        response = respond(request)
                        if response is not None:
                            connection.sendall((json.dumps(response) + "\n").encode())
            except Exception as error:
                errors.append(error)
            finally:
                server.close()
                path.unlink(missing_ok=True)

        thread = threading.Thread(target=work)
        thread.start()
        self.addCleanup(lambda: thread.join(timeout=6))
        return str(path), calls, errors

    def invoke_tty(self, command, args, color, term, columns=None):
        master, slave = pty.openpty()
        if columns is not None:
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, columns, 0, 0))
        env = {**self.fixture.env, "TERM": term}
        if color:
            env.pop("NO_COLOR")
        child = subprocess.Popen([*command, *args], env=env, cwd=self.fixture.repo,
                                 stdin=subprocess.DEVNULL, stdout=slave, stderr=subprocess.PIPE)
        os.close(slave)
        try:
            output = bytearray()
            deadline = time.monotonic() + 10
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                    child.kill()
                    child.wait()
                    raise TimeoutError("TTY command did not exit")
                try:
                    chunk = os.read(master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not chunk:
                    break
                output.extend(chunk)
            _, stderr = child.communicate(timeout=10)
            return child.returncode, bytes(output), stderr
        finally:
            os.close(master)
            if child.poll() is None:
                child.kill()
                child.wait()


class CliTests(NativeCase):
    def test_all_help_pages_and_topics_match_at_multiple_widths(self):
        with gzip.open(PROJECT / "tests/fixtures/help.json.gz", "rt") as source:
            pages = json.load(source)
        for page in pages:
            args = ["--help"] if page["topic"] is None else ["help", page["topic"]]
            actual = self.invoke([str(self.native)], args, COLUMNS=str(page["columns"]))
            self.assertEqual(actual, (0, page["text"] + "\n", ""), f"{args}, width {page['columns']}")

    def test_help_precedence_and_invalid_topics(self):
        for args in [[], ["history", "--help"], ["history", "--wat", "--help"],
                     ["help", "history", "--json"], ["help", "not-a-command"],
                     ["help", "not-a-command", "--json"], ["help", "history", "extra"],
                     ["help", "history", "extra", "--help"]]:
            self.compare(args)

    def test_empty_history_and_completion_are_readonly(self):
        for args in [["history"], ["history", "--json"], ["completion", "bash"]]:
            self.compare(args)
        self.assertFalse(self.fixture.state.exists())

    def test_populated_history_metadata_and_narrow_formatting(self):
        bench.seed_history(self.fixture, 3)
        for columns in [1, 16, 24, 40, 60, 80, 120]:
            self.compare(["history"], COLUMNS=str(columns))
        result = self.compare(["history", "--json"])
        summaries = result[1]["data"]["retained"]
        self.assertEqual(sum(item["count"] for item in summaries), 27)
        self.assertTrue(all(item["oldest"] == "2026-01-01T00:00:00.123Z" for item in summaries))

    def test_legacy_state_and_equivalent_alias_are_preserved(self):
        bench.seed_history(self.fixture, 1)
        legacy = self.fixture.state.with_name("devtree")
        self.fixture.state.rename(legacy)
        self.compare(["history", "--json"])
        self.fixture.state.symlink_to("devtree")
        self.compare(["history", "--json"])

    def test_split_state_directories_are_refused(self):
        self.fixture.state.mkdir(parents=True, mode=0o700)
        self.fixture.state.with_name("devtree").mkdir(mode=0o700)
        result = self.compare(["history", "--json"])
        self.assertEqual(result[0], 1)
        self.assertEqual(result[1]["error"]["code"], "STATE_UNAVAILABLE")

    def test_nonprivate_or_symlinked_categories_are_refused(self):
        bench.seed_history(self.fixture, 1)
        category = self.fixture.state / "removed"
        category.chmod(0o755)
        self.assertEqual(self.compare(["history", "--json"])[0], 1)
        category.chmod(0o700)
        moved = self.fixture.state / "other"
        category.rename(moved)
        category.symlink_to("other")
        self.assertEqual(self.compare(["history", "--json"])[0], 1)

    def test_log_task_permissions_and_symlinks_are_checked(self):
        bench.seed_history(self.fixture, 1)
        task = self.fixture.state / "logs/bench--item-000"
        task.chmod(0o755)
        self.assertEqual(self.compare(["history", "--json"])[0], 1)
        task.chmod(0o700)
        self.compare(["history", "--json"])

    def test_history_never_reads_retained_file_contents(self):
        bench.seed_history(self.fixture, 1)
        secret = self.fixture.state / "env-backups/record"
        secret.write_text("DO_NOT_DISPLAY=synthetic-private-value\n")
        secret.chmod(0)
        expected = self.invoke(self.reference, ["history", "--json"])
        result = self.invoke([str(self.native)], ["history", "--json"])
        self.assertEqual(result, expected)
        self.assertNotIn("DO_NOT_DISPLAY", json.dumps(result))
        secret.chmod(0o600)

    def test_size_rounding_matches_javascript_at_exact_ties(self):
        category = self.fixture.state / "removed"
        category.mkdir(parents=True, mode=0o700)
        for size in [1280, 1792, 1024]:
            (category / "record").write_bytes(b"x" * size)
            self.compare(["history"])

    def test_filesystem_timestamps_match_javascript_millisecond_conversion(self):
        category = self.fixture.state / "removed"
        category.mkdir(parents=True, mode=0o700)
        file = category / "record"
        file.write_bytes(b"x")
        os.utime(file, ns=(1767225600122999906, 1767225600122999906))
        result = self.compare(["history", "--json"])
        self.assertEqual(result[1]["data"]["retained"][0]["oldest"], "2026-01-01T00:00:00.123Z")

    def test_native_runtime_needs_no_bun_or_external_commands(self):
        bench.seed_history(self.fixture, 1)
        for args in [["history", "--json"], ["--help"], ["completion", "bash"]]:
            expected = self.invoke(self.reference, args)
            actual = self.invoke([str(self.native)], args, PATH="/no-executables-here")
            self.assertEqual(actual, expected)

    def test_tty_heading_colors_and_json_match(self):
        for color, term in [(True, "xterm"), (False, "xterm"), (True, "dumb")]:
            for args in [["history"], ["history", "--json"]]:
                actual = self.invoke_tty([str(self.native)], args, color, term)
                expected = self.invoke_tty(self.reference, args, color, term)
                # JSON object-key order is not part of the machine-readable contract.
                if "--json" in args:
                    self.assertEqual(json.loads(actual[1]), json.loads(expected[1]))
                    self.assertNotIn(b"\x1b", actual[1])
                    self.assertEqual((actual[0], actual[2]), (expected[0], expected[2]))
                else:
                    self.assertEqual(actual, expected)

    def test_unknown_commands_and_invalid_workers_fail_without_effects_or_fallback(self):
        for command in ["unknown-command"]:
            before = bench.readonly_snapshot(self.fixture)
            result = self.invoke([str(self.native)], [command, "--json"])
            self.assertEqual(result[0], 1)
            self.assertEqual(result[1]["error"]["code"], "INVALID_ARGUMENTS")
            self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        for command in ["_startup", "_event"]:
            before = bench.readonly_snapshot(self.fixture)
            result = self.invoke([str(self.native)], [command, "--json"])
            self.assertEqual(result[0], 1)
            self.assertEqual(result[1]["error"]["code"], "INVALID_ARGUMENTS")
            result = self.invoke([str(self.native)], [command])
            self.assertEqual(result[0], 1)
            self.assertNotIn("does not implement", result[2])
            self.assertEqual(bench.readonly_snapshot(self.fixture), before)
        self.assertFalse(self.fixture.state.exists())

    def test_history_rejects_extra_arguments_and_completion_rejects_json(self):
        for args in [["history", "extra"], ["history", "extra", "--json"], ["completion", "bash", "--json"], ["completion", "zsh"]]:
            self.compare(args)

    def test_argument_parser_dash_values_short_groups_and_missing_values_match(self):
        for args in [["status", "--project", "--json"], ["create", "bench", "x", "--branch", "-x"],
                     ["status", "--json=yes"], ["status", "-abc"], ["status", "-z"], ["--project"],
                     ["--project", "--help"], ["status", "-hh"], ["status", "-"],
                     ["status", "--project", "--"], ["create", "bench", "x", "--branch"],
                     ["create", "bench", "x", "--branch", "--help"], ["--project", "--json"]]:
            self.compare(args)


if __name__ == "__main__":
    unittest.main()
