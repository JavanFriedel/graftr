#!/usr/bin/env python3
"""Language-neutral, isolated Linux CLI benchmarks. See docs/benchmarks.md."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from status_fixture import seed_status


SUITE = "graftr-standalone-v1"
HISTORY_SUITE = "graftr-history-v1"
STATUS_SUITE = "graftr-status-v1"
PROJECT = Path(__file__).resolve().parents[1]
METRICS = ("wall_ms", "cpu_ms", "max_rss_kib")
METHODOLOGY = {
    "clock": "perf_counter_ns around GNU time + command; fresh process per invocation",
    "cpu": "wait4 user + system, including GNU time and waited-for descendants",
    "memory": "GNU time %M on Linux; maximum individual process high-water, not tree sum",
    "cache": "uncontrolled warm filesystem caches; no cache flushing",
    "stdio": "non-TTY, stdin closed, stdout/stderr captured in temporary files",
}


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def measure(command, cwd, env, timeout=60):
    """A native meter avoids inherited Python RSS becoming a floor for native CLIs."""
    with tempfile.TemporaryFile(dir=cwd) as stdout, tempfile.TemporaryFile(dir=cwd) as stderr, \
            tempfile.NamedTemporaryFile(dir=env.get("TMPDIR", cwd)) as rss:
        started = time.perf_counter_ns()
        child = subprocess.Popen(["/usr/bin/time", "-q", "-f", "%M", "-o", rss.name, "--", *command], cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                                 stdout=stdout, stderr=stderr, start_new_session=True)

        def expired(_signal, _frame):
            raise TimeoutError(f"Command exceeded {timeout}s: {command}")

        previous = signal.signal(signal.SIGALRM, expired)
        try:
            signal.setitimer(signal.ITIMER_REAL, timeout)
            _, status, usage = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
        except BaseException:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            _, status, _ = os.wait4(child.pid, 0)
            child.returncode = os.waitstatus_to_exitcode(status)
            raise
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            signal.signal(signal.SIGALRM, previous)
        elapsed = (time.perf_counter_ns() - started) / 1_000_000
        rss.seek(0)
        peak_rss = int(rss.read().decode("ascii").strip())
        captured = []
        for stream in (stdout, stderr):
            require(stream.tell() <= 4 * 1024 * 1024, "Command output exceeded 4 MiB")
            stream.seek(0)
            captured.append(stream.read().decode("utf-8"))
        return {
            "wall_ms": elapsed,
            "cpu_ms": (usage.ru_utime + usage.ru_stime) * 1000,
            "max_rss_kib": peak_rss,
        }, {"exit_code": child.returncode, "stdout": captured[0], "stderr": captured[1]}


def normalize(value, root, key=None):
    # Only explicitly volatile fields are masked; statuses, ordering and errors remain.
    if key in ("timings", "installKey") and value is not None:
        return f"<{key}>"
    if isinstance(value, dict):
        return {k: normalize(v, root, k) for k, v in value.items()}
    if isinstance(value, list):
        result = [normalize(v, root) for v in value]
        # Receipt filenames are path hashes, so their enumeration order is incidental.
        return sorted(result, key=lambda v: (v["path"], v.get("socket") or "")) if key == "setups" else result
    if isinstance(value, str):
        value = value.replace(str(root), "<fixture>")
        value = re.sub(r"\b\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z\b", "<timestamp>", value)
        value = re.sub(r"\b[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\b", "<uuid>", value)
        value = re.sub(r"\b\d{8}T\d{9}Z-([a-z0-9-]+)-[0-9a-f]{6}\.log\b", r"<timestamp>-\1-<random>.log", value)
    return value


class Fixture:
    def __init__(self, root, command, timeout):
        self.root, self.command, self.timeout = root, command, timeout
        self.repo = root / "repo"
        self.checkout = root / "trees/sample"
        self.state = root / "state/graftr"
        for directory in (self.repo, root / "home", root / "config/graftr", root / "bin", root / "tmp"):
            directory.mkdir(parents=True, mode=0o700)
        # Do not inherit caller workspaces, plugin contexts, Git overrides or secrets.
        self.env = {
            "PATH": f"{root / 'bin'}:{os.environ.get('PATH', '/usr/bin:/bin')}",
            "HOME": str(root / "home"), "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_STATE_HOME": str(root / "state"), "XDG_DATA_HOME": str(root / "data"),
            "XDG_CACHE_HOME": str(root / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime"),
            "TMPDIR": str(root / "tmp"), "NO_COLOR": "1", "TERM": "dumb", "COLUMNS": "120",
            "LANG": "C", "LC_ALL": "C", "TZ": "UTC", "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
            "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
        }
        for executable in ("systemctl", "herdr", "tailscale", "psql", "createdb", "dropdb"):
            stub = root / "bin" / executable
            stub.write_text('#!/bin/sh\nprintf "%s\\n" "$0" >> "$HOME/forbidden-service"\nexit 99\n')
            stub.chmod(0o700)
        self.git("init", "-q", "-b", "main", str(self.repo))
        (self.repo / ".gitignore").write_text(".env\nnode_modules/\n")
        (self.repo / "tracked.txt").write_text("committed\n")
        (self.repo / "package.json").write_text('{"name":"graftr-benchmark","private":true}\n')
        self.git("add", ".")
        self.git("-c", "user.name=Benchmark", "-c", "user.email=benchmark@example.test", "commit", "-qm", "fixture")
        self.commit = self.git("rev-parse", "HEAD").strip()
        self.baseline = "SECRET=synthetic-initial\n"
        (self.repo / ".env").write_text(self.baseline)
        profile = {
            "repo": str(self.repo), "worktreeRoot": str(root / "trees"),
            "envSource": str(self.repo / ".env"), "localPorts": [40000, 60000],
            "envOverrides": {"PORT": "{{localPort}}"},
            "installCommand": ["/bin/sh", "-c", "mkdir -p node_modules; count=0; if [ -f node_modules/install-count ]; then read -r count < node_modules/install-count; fi; printf '%s\\n' \"$((count + 1))\" > node_modules/install-count; printf installed > node_modules/fixture"],
            "installOutputs": ["node_modules/fixture"],
        }
        (root / "config/graftr/projects.json").write_text(json.dumps({
            "schemaVersion": 2, "herdr": "off", "startupThaw": False, "projects": {"bench": profile},
        }))

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repo), *args], env=self.env, text=True, stderr=subprocess.PIPE)

    def invoke(self, args, expected_exit=0, cwd=None):
        metrics, result = measure([*self.command, *args], cwd or self.repo, self.env, self.timeout)
        require(not (self.root / "home/forbidden-service").exists(), "Forbidden external service was invoked")
        require(result["exit_code"] == expected_exit, f"Unexpected exit for {args}: {result}")
        require(not result["stderr"], f"Unexpected stderr for {args}: {result['stderr']}")
        contract = dict(result)
        if "--json" in args:
            payload = json.loads(result["stdout"])
            require(payload["ok"] == (expected_exit == 0), f"Unexpected JSON outcome: {payload}")
            require(payload["outcome"] == ("completed" if expected_exit == 0 else "failed"), "Operation did not finish as expected")
            contract["stdout"] = payload
        return metrics, normalize(contract, self.root)

    def ready(self, installs):
        require(self.checkout.is_dir(), "Checkout was not created")
        require(self.git("-C", str(self.checkout), "rev-parse", "HEAD").strip() == self.commit, "Commit changed")
        require(self.git("-C", str(self.checkout), "branch", "--show-current").strip() == "task/sample", "Branch changed")
        require(self.baseline.strip() in (self.checkout / ".env").read_text(), "Environment was not regenerated")
        require((self.checkout / "node_modules/fixture").read_text() == "installed", "Dependencies were not installed")
        require(int((self.checkout / "node_modules/install-count").read_text()) == installs, "Unexpected installer execution count")
        task = json.loads((self.state / "bench--sample.json").read_text())
        require(task["status"] == "ready" and task["path"] == str(self.checkout), "Task is not ready")
        require(task["localPort"] == 40000, "Fixture port 40000 is busy; rerun when it is available")


def summarize(samples):
    result = {}
    for metric in METRICS:
        values = sorted(sample[metric] for sample in samples)
        result[metric] = {"median": statistics.median(values), "p95": values[math.ceil(len(values) * .95) - 1],
                          "min": values[0], "max": values[-1]}
    return result


def record(cases, fixture, name, args, measured, expected_exit=0, cwd=None):
    metrics, contract = fixture.invoke(args, expected_exit, cwd)
    if measured:
        case = cases.setdefault(name, {"argv": args, "samples": [], "contracts": []})
        case["samples"].append(metrics)
        if contract not in case["contracts"]:
            case["contracts"].append(contract)
    return contract["stdout"]


def lifecycle(cases, fixture, measured):
    def step(name, args, expected_exit=0, cwd=None):
        return record(cases, fixture, name, [*args, "--json"], measured, expected_exit, cwd)

    step("create", ["create", "bench", "sample", "--no-herdr"])
    fixture.ready(installs=1)
    step("setup-reuse", ["setup", "--no-herdr"], cwd=fixture.checkout)
    fixture.ready(installs=1)
    step("setup-install", ["setup", "--no-herdr", "--install"], cwd=fixture.checkout)
    fixture.ready(installs=2)
    step("open", ["open", "bench--sample", "--no-herdr"])
    fixture.ready(installs=2)
    require(step("cleanup", ["cleanup", "bench--sample"])["data"]["status"] == "cleaned", "Cleanup did not finish")
    require(step("close", ["close", "bench--sample"])["data"]["status"] == "closed", "Close did not finish")
    step("freeze", ["freeze", "bench--sample", "--yes", "--note", "Benchmark return"])
    require(not fixture.checkout.exists() and not (fixture.state / "bench--sample.json").exists(), "Freeze did not remove checkout/active record")
    require(fixture.git("rev-parse", "task/sample").strip() == fixture.commit, "Freeze lost branch")
    frozen = step("frozen-json", ["frozen"])
    require(len(frozen["data"]["frozen"]) == 1 and frozen["data"]["frozen"][0]["status"] == "frozen", "Missing frozen recovery record")
    fixture.baseline = "SECRET=synthetic-refreshed\n"
    (fixture.repo / ".env").write_text(fixture.baseline)
    step("thaw", ["thaw", "bench--sample", "--no-herdr"])
    fixture.ready(installs=1)
    (fixture.checkout / "tracked.txt").write_text("unsaved benchmark work\n")
    refused = step("remove-dirty", ["remove", "bench--sample", "--yes"], expected_exit=1)
    require(refused["error"]["phase"] == "preflight", "Dirty work was not refused at preflight")
    require((fixture.checkout / "tracked.txt").read_text() == "unsaved benchmark work\n", "Dirty work was changed")
    (fixture.checkout / "tracked.txt").write_text("committed\n")
    step("remove", ["remove", "bench--sample", "--yes"])
    require(not fixture.checkout.exists() and not (fixture.state / "bench--sample.json").exists(), "Remove left checkout/active record")
    require(fixture.git("rev-parse", "task/sample").strip() == fixture.commit, "Remove lost branch")
    require(len(fixture.git("worktree", "list", "--porcelain").split("worktree ")) == 2, "Linked Git worktree registration remains")


def command_argv(value):
    command = json.loads(value)
    if not isinstance(command, list) or not command or any(not isinstance(arg, str) or not arg for arg in command):
        raise argparse.ArgumentTypeError("command must be a nonempty JSON array of strings")
    executable = shutil.which(command[0])
    if not executable:
        raise argparse.ArgumentTypeError(f"executable not found: {command[0]}")
    return [str(Path(executable).resolve()), *command[1:]]


def fingerprint(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def seed_history(fixture, tasks):
    """Fixed synthetic retained metadata; no implementation provisions this workload."""
    fixture.state.mkdir(parents=True, mode=0o700)
    directories = ["removed", "timings", "env-backups", "migration-backups", "backups",
                   "creating", "hook-work", "tailscale-hooks", "postgres-hooks"]
    for index, name in enumerate(directories):
        category = fixture.state / name
        category.mkdir(mode=0o700)
        (category / "record").write_bytes(b"x" * (17 + index))
        nested = category / "nested"
        nested.mkdir(mode=0o700)
        (nested / "payload").write_bytes(b"y" * (2048 + index))
        (nested / "link").symlink_to("../record")
        (category / "ignored-link").symlink_to("record")
    logs = fixture.state / "logs"
    logs.mkdir(mode=0o700)
    for index in range(tasks):
        task = logs / f"bench--item-{index:03}"
        task.mkdir(mode=0o700)
        for run in range(3):
            (task / f"run-{run}.log").write_bytes(b"log" * (index + run + 1))
    (logs / "ignored-task-link").symlink_to("bench--item-000")
    # Reset after population so directory mtimes are as deterministic as file mtimes.
    for path in [fixture.state, *fixture.state.rglob("*")]:
        os.utime(path, ns=(1767225600123000000, 1767225600123000000), follow_symlinks=False)


def readonly_snapshot(fixture):
    result = {}
    for name in ("state", "config", "home", "data", "cache", "runtime"):
        directory = fixture.root / name
        if directory.exists():
            for path in [directory, *directory.rglob("*")]:
                metadata = path.lstat()
                content = os.readlink(path) if path.is_symlink() else fingerprint(path) if path.is_file() else None
                result[str(path.relative_to(fixture.root))] = (metadata.st_mode, metadata.st_mtime_ns, content)
    return result


def workload(suite, tasks):
    if suite == "status":
        dimensions = {"fixture": "synthetic recorded state; no linked checkouts", "task_records": tasks,
                      "setup_records": tasks + 5, "adopted_records": 2, "recovery_records": 5,
                      "hook_plan_redaction": "cleanup+retire", "transpiler_cache": "fixture temporary directory"}
    elif suite == "history":
        dimensions = {"log_task_directories": tasks, "retained_entries": 18 + tasks * 3,
                      "transpiler_cache": "fixture temporary directory"}
    else:
        dimensions = {"populated_tasks": tasks, "installer": "local marker; no downloads"}
    return {**dimensions, "herdr": "off", "timers": "none"}


def run_suite(args):
    require(sys.platform == "linux" and Path("/usr/bin/time").is_file(), "Benchmarks require Linux and GNU /usr/bin/time")
    cases = {}
    started = datetime.now(timezone.utc).isoformat()
    with tempfile.TemporaryDirectory(prefix="graftr-benchmark-", dir=tempfile.gettempdir()) as directory:
        root = Path(directory)
        empty = Fixture(root / "empty", args.command, args.timeout)
        populated = Fixture(root / "populated", args.command, args.timeout)
        meter_samples = [measure(["/bin/true"], empty.repo, empty.env, args.timeout)[0] for _ in range(args.runs)]
        if args.suite in ("history", "status"):
            for fixture in (empty, populated):
                # Runtime cache writes are not application mutations; keep them beside meter files.
                fixture.env["BUN_RUNTIME_TRANSPILER_CACHE_PATH"] = str(fixture.root / "tmp/transpiler-cache")
            if args.suite == "history":
                seed_history(populated, args.tasks)
                inspections = [("help", empty, ["--help"]),
                               ("history-empty", empty, ["history"]),
                               ("history-empty-json", empty, ["history", "--json"]),
                               ("history-populated", populated, ["history"]),
                               ("history-populated-json", populated, ["history", "--json"]),
                               ("completion-script", empty, ["completion", "bash"])]
            else:
                seed_status(populated, args.tasks)
                inspections = [("status-empty", empty, ["status"]),
                               ("status-empty-json", empty, ["status", "--json"]),
                               ("status-populated", populated, ["status"]),
                               ("status-populated-json", populated, ["status", "--json"]),
                               ("frozen-empty", empty, ["frozen"]),
                               ("frozen-empty-json", empty, ["frozen", "--json"]),
                               ("frozen-populated", populated, ["frozen"]),
                               ("frozen-populated-json", populated, ["frozen", "--json"])]
            snapshots = [readonly_snapshot(fixture) for fixture in (empty, populated)]
        else:
            for index in range(args.tasks):
                populated.invoke(["create", "bench", f"item-{index:03}", "--no-herdr", "--json"])
            inspections = [
                ("help", empty, ["--help"]),
                ("status-empty", empty, ["status"]),
                ("status-empty-json", empty, ["status", "--json"]),
                ("status-populated", populated, ["status"]),
                ("status-populated-json", populated, ["status", "--json"]),
                ("completion", populated, ["_complete", "open", "item-"]),
            ]
        for iteration in range(args.warmups + args.runs):
            measured = iteration >= args.warmups
            print(f"{'Sample' if measured else 'Warmup'} {iteration - args.warmups + 1 if measured else iteration + 1}", file=sys.stderr)
            for name, fixture, argv in inspections:
                output = record(cases, fixture, name, argv, measured)
                if name == "status-populated-json":
                    require(len(output["data"]["tasks"]) == args.tasks, "Populated status omitted tasks")
                    if args.suite == "status":
                        require(len(output["data"]["setups"]) == args.tasks + 5 and len(output["data"]["adopted"]) == 2 and len(output["data"]["frozen"]) == 4, "Status omitted setup/adoption/recovery records")
                        require("synthetic-private-argument" not in json.dumps(output) and "synthetic-private-context" not in json.dumps(output), "Status exposed private hook commands/context")
                    else:
                        require(sorted(task["localPort"] for task in output["data"]["tasks"]) == list(range(40000, 40000 + args.tasks)), "Fixture ports are busy; rerun when they are available")
                elif name == "completion":
                    require(set(output.splitlines()) == {f"item-{index:03}" for index in range(args.tasks)}, "Completion omitted tasks")
                elif name == "history-populated-json":
                    summaries = output["data"]["retained"]
                    require(len(summaries) == 10 and sum(item["count"] for item in summaries) == 18 + args.tasks * 3, "History omitted retained entries")
                elif name == "frozen-populated-json":
                    require(len(output["data"]["frozen"]) == 4, "Frozen listing omitted recovery records")
            if args.suite in ("history", "status"):
                require([readonly_snapshot(fixture) for fixture in (empty, populated)] == snapshots, "Read-only commands changed fixture config/state")
            else:
                with tempfile.TemporaryDirectory(prefix="cycle-", dir=root) as cycle:
                    lifecycle(cases, Fixture(Path(cycle), args.command, args.timeout), measured)
    for case in cases.values():
        case["summary"] = summarize(case["samples"])
        require(len(case["contracts"]) == 1, "Outputs varied between repetitions after normalization; inspect normalization/workload before comparing implementations")
    try:
        revision = subprocess.check_output(["git", "-C", str(PROJECT), "rev-parse", "HEAD"], text=True).strip()
    except subprocess.CalledProcessError:
        revision = None
    return {
        "schema_version": 1, "suite": {"history": HISTORY_SUITE, "status": STATUS_SUITE, "standalone": SUITE}[args.suite], "label": args.label, "started_at": started,
        "workload": workload(args.suite, args.tasks),
        "methodology": METHODOLOGY, "runs": args.runs, "warmups": args.warmups,
        "instrumentation_floor": {"command": ["/bin/true"], "samples": meter_samples, "summary": summarize(meter_samples)},
        "command": args.command,
        "environment": {"platform": platform.platform(), "machine": platform.machine(), "hostname": platform.node(),
                        "cpu_count": os.cpu_count(), "python": platform.python_version(),
                        "git": subprocess.check_output(["git", "--version"], text=True).strip()},
        "implementation": {"runner_revision": revision, "runner_sha256": fingerprint(Path(__file__)),
                           "status_fixture_sha256": fingerprint(Path(__file__).with_name("status_fixture.py")) if args.suite == "status" else None,
                            "checkout_source_sha256": hashlib.sha256(b"".join(p.relative_to(PROJECT).as_posix().encode() + p.read_bytes() for p in sorted((PROJECT / "src").rglob("*.rs")))).hexdigest(),
                           "command_files": [{"path": arg, "bytes": Path(arg).stat().st_size, "sha256": fingerprint(Path(arg))}
                                             for arg in args.command if Path(arg).is_file()]},
        "cases": cases,
    }


def comparison(baseline, candidate):
    for key in ("schema_version", "suite", "workload", "methodology"):
        require(baseline[key] == candidate[key], f"Cannot compare different {key}")
    require(baseline["schema_version"] == 1, "Unsupported benchmark result schema")
    require(baseline["cases"].keys() == candidate["cases"].keys(), "Benchmark case sets differ")
    rows = []
    for name, before in baseline["cases"].items():
        after = candidate["cases"][name]
        contracts_match = sorted(json.dumps(v, sort_keys=True) for v in before["contracts"]) == sorted(json.dumps(v, sort_keys=True) for v in after["contracts"])
        require(before["argv"] == after["argv"], f"Benchmark arguments differ: {name}")
        require(before["samples"] and after["samples"], f"Missing samples: {name}")
        # Recompute instead of trusting a saved summary when reporting ratios.
        left, right = summarize(before["samples"]), summarize(after["samples"])
        ratios = {metric: right[metric]["median"] / left[metric]["median"] if left[metric]["median"] else None for metric in METRICS}
        rows.append({"case": name, "contracts_match": contracts_match, "ratios": ratios})
    warnings = []
    if baseline["environment"] != candidate["environment"]:
        warnings.append("Host/toolchain metadata differs; timing ratios may not be comparable.")
    if (baseline["runs"], baseline["warmups"]) != (candidate["runs"], candidate["warmups"]):
        warnings.append("Sample/warmup counts differ.")
    return {"contracts_match": all(row["contracts_match"] for row in rows), "warnings": warnings, "cases": rows}


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def nonnegative(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be at least 0")
    return number


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="action", required=True)
    run = subcommands.add_parser("run", help="measure an isolated CLI and save a result")
    run.add_argument("--command", type=command_argv, help='JSON argv, e.g. ["/absolute/path/graftr"]')
    run.add_argument("--label", default="native")
    run.add_argument("--suite", choices=["standalone", "history", "status"], default="standalone", help="full lifecycle, history/help, or mixed-record status/frozen inspection")
    run.add_argument("--output", type=Path, required=True, help="new JSON result file; existing files are never overwritten")
    run.add_argument("--runs", type=positive, default=10)
    run.add_argument("--warmups", type=nonnegative, default=2)
    run.add_argument("--tasks", type=positive, default=25)
    run.add_argument("--timeout", type=positive, default=60, help="seconds per command")
    compare = subcommands.add_parser("compare", help="compare outputs and median performance (candidate / baseline)")
    compare.add_argument("baseline", type=Path)
    compare.add_argument("candidate", type=Path)
    compare.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.action == "run":
            if args.command is None:
                args.command = command_argv(json.dumps([str(PROJECT / "dist/graftr")]))
            # Claim the output before doing any work; a failed run leaves no passing artifact.
            with open(args.output, "x", opener=lambda path, flags: os.open(path, flags, 0o600)) as output:
                try:
                    result = run_suite(args)
                    json.dump(result, output, indent=2)
                    output.write("\n")
                except BaseException:
                    args.output.unlink()
                    raise
            print(f"{'CASE':<24} {'MEDIAN ms':>10} {'P95 ms':>10} {'PEAK MiB*':>10}")
            for name, case in result["cases"].items():
                summary = case["summary"]
                print(f"{name:<24} {summary['wall_ms']['median']:>10.2f} {summary['wall_ms']['p95']:>10.2f} {summary['max_rss_kib']['max'] / 1024:>10.1f}")
            print(f"* Maximum individual-process RSS, not aggregate tree memory.\nSaved {args.output}; fixtures removed.")
            return 0
        result = comparison(json.loads(args.baseline.read_text()), json.loads(args.candidate.read_text()))
        if args.json:
            print(json.dumps(result, indent=2))
        else:
            for warning in result["warnings"]:
                print(f"Warning: {warning}")
            print("Ratios are candidate / baseline; below 1.00 is smaller/faster.")
            print(f"{'CASE':<24} {'WALL':>8} {'CPU':>8} {'RSS':>8}  OUTPUT")
            for row in result["cases"]:
                ratios = [f"{row['ratios'][metric]:.2f}x" if row['ratios'][metric] is not None else "n/a" for metric in METRICS]
                print(f"{row['case']:<24} {ratios[0]:>8} {ratios[1]:>8} {ratios[2]:>8}  {'match' if row['contracts_match'] else 'DIFF'}")
        return 0 if result["contracts_match"] else 1
    except (OSError, ValueError, RuntimeError, TimeoutError, subprocess.SubprocessError) as error:
        print(f"benchmark: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
