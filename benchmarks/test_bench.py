import copy
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

import bench


class BenchmarkTests(unittest.TestCase):
    def test_measure_reaps_each_child_separately_and_captures_output(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            metrics, output = bench.measure([sys.executable, "-c", "print('measured')"], Path(directory), {})
            self.assertEqual(output, {"exit_code": 0, "stdout": "measured\n", "stderr": ""})
            self.assertGreater(metrics["wall_ms"], 0)
            self.assertGreater(metrics["max_rss_kib"], 0)
            _, failure = bench.measure([sys.executable, "-c", "raise SystemExit(3)"], Path(directory), {})
            self.assertEqual(failure["exit_code"], 3)

    def test_timeout_kills_and_reaps_the_command(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            with self.assertRaises(TimeoutError):
                bench.measure([sys.executable, "-c", "import time; time.sleep(60)"], Path(directory), {}, .05)
            # A timeout must not poison subsequent measurements or leave an alarm set.
            bench.measure([sys.executable, "-c", "pass"], Path(directory), {})

    def test_meter_does_not_inherit_the_python_runners_memory_floor(self):
        allocation = bytearray(64 * 1024 * 1024)
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            metrics, _ = bench.measure(["/bin/true"], Path(directory), {})
        self.assertLess(metrics["max_rss_kib"], len(allocation) / 1024 / 2)

    def test_normalization_masks_only_documented_volatility(self):
        value = {
            "path": "/fixture/trees/sample", "status": "ready", "timings": {"totalMs": 123},
            "installKey": "path-dependent", "runId": "41b4d1ec-0423-43e1-8246-423da46afc1d",
            "startedAt": "2026-11-02T19:14:41.377Z", "baseCommit": "f31a5d3a971e845b828857c3b9e3b6b6894d7ea4",
            "installLog": "/fixture/state/logs/20261102T191441377Z-dependencies-abcdef.log",
            "setups": [{"path": "/fixture/b"}, {"path": "/fixture/a"}], "progress": ["second", "first"],
        }
        normalized = bench.normalize(value, Path("/fixture"))
        self.assertEqual(normalized["path"], "<fixture>/trees/sample")
        self.assertEqual(normalized["timings"], "<timings>")
        self.assertEqual(normalized["installKey"], "<installKey>")
        self.assertEqual(normalized["runId"], "<uuid>")
        self.assertEqual(normalized["startedAt"], "<timestamp>")
        self.assertEqual(normalized["baseCommit"], value["baseCommit"])
        self.assertEqual(normalized["status"], "ready")
        self.assertEqual(normalized["installLog"], "<fixture>/state/logs/<timestamp>-dependencies-<random>.log")
        self.assertEqual(normalized["setups"][0]["path"], "<fixture>/a")
        self.assertEqual(normalized["progress"], ["second", "first"])
        self.assertEqual(bench.normalize(None, Path("/fixture"), "installKey"), None)

    def test_summary_uses_nearest_rank_p95(self):
        summary = bench.summarize([dict.fromkeys(bench.METRICS, number) for number in range(1, 21)])
        self.assertEqual(summary["wall_ms"], {"median": 10.5, "p95": 19, "min": 1, "max": 20})

    def result(self):
        return {
            "schema_version": 1, "suite": bench.SUITE, "workload": {}, "methodology": bench.METHODOLOGY,
            "environment": {}, "runs": 2, "warmups": 1,
            "cases": {"help": {"argv": ["--help"], "contracts": [{"stdout": "help", "exit_code": 0}],
                               "samples": [dict.fromkeys(bench.METRICS, 10)]}},
        }

    def test_compare_uses_raw_samples_and_detects_output_differences(self):
        baseline = self.result()
        candidate = copy.deepcopy(baseline)
        candidate["cases"]["help"]["samples"][0]["wall_ms"] = 5
        candidate["cases"]["help"]["summary"] = {"wall_ms": {"median": 999}}
        self.assertEqual(bench.comparison(baseline, candidate)["cases"][0]["ratios"]["wall_ms"], .5)
        candidate["cases"]["help"]["contracts"][0]["stdout"] = "different help"
        self.assertFalse(bench.comparison(baseline, candidate)["contracts_match"])

    def test_compare_rejects_different_workloads_or_missing_cases(self):
        baseline = self.result()
        for key, value in (("suite", "other"), ("workload", {"tasks": 2}), ("cases", {})):
            candidate = copy.deepcopy(baseline)
            candidate[key] = value
            with self.assertRaises(RuntimeError):
                bench.comparison(baseline, candidate)

    def test_compare_warns_about_different_environment_and_sample_counts(self):
        baseline = self.result()
        candidate = copy.deepcopy(baseline)
        candidate["environment"] = {"hostname": "other"}
        candidate["runs"] = 3
        self.assertEqual(len(bench.comparison(baseline, candidate)["warnings"]), 2)

    def test_two_independent_cli_runs_match_and_remove_their_fixtures(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            root = Path(directory)
            before = set(Path(tempfile.gettempdir()).glob("graftr-benchmark-*"))
            files = [root / "before.json", root / "after.json"]
            for output in files:
                child = subprocess.run([sys.executable, str(Path(bench.__file__)), "run", "--runs", "1", "--warmups", "0",
                                        "--tasks", "2", "--output", str(output)], capture_output=True, text=True)
                self.assertEqual(child.returncode, 0, child.stderr)
                self.assertEqual(output.stat().st_mode & 0o777, 0o600)
            baseline, candidate = [json.loads(path.read_text()) for path in files]
            self.assertTrue(bench.comparison(baseline, candidate)["contracts_match"])
            self.assertEqual(len(baseline["cases"]), 17)
            self.assertEqual(set(Path(tempfile.gettempdir()).glob("graftr-benchmark-*")), before)
            existing = files[0].read_bytes()
            child = subprocess.run([sys.executable, str(Path(bench.__file__)), "run", "--output", str(files[0])], capture_output=True)
            self.assertEqual(child.returncode, 1)
            self.assertEqual(files[0].read_bytes(), existing)

    def test_failed_run_removes_output_and_fixture(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            output = Path(directory) / "failed.json"
            before = set(Path(tempfile.gettempdir()).glob("graftr-benchmark-*"))
            child = subprocess.run([sys.executable, str(Path(bench.__file__)), "run", "--runs", "1", "--warmups", "0",
                                    "--tasks", "1", "--command", json.dumps([shutil.which("false")]), "--output", str(output)],
                                   capture_output=True, text=True)
            self.assertEqual(child.returncode, 1)
            self.assertFalse(output.exists())
            self.assertEqual(set(Path(tempfile.gettempdir()).glob("graftr-benchmark-*")), before)

    def test_history_suite_has_its_own_contract_and_preserves_fixture_state(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            output = Path(directory) / "history.json"
            child = subprocess.run([sys.executable, str(Path(bench.__file__)), "run", "--suite", "history",
                                    "--runs", "2", "--warmups", "1", "--tasks", "2", "--output", str(output)],
                                   capture_output=True, text=True)
            self.assertEqual(child.returncode, 0, child.stderr)
            result = json.loads(output.read_text())
            self.assertEqual(result["suite"], bench.HISTORY_SUITE)
            self.assertEqual(len(result["cases"]), 6)
            self.assertTrue(all(len(case["samples"]) == 2 and len(case["contracts"]) == 1 for case in result["cases"].values()))
            self.assertTrue(bench.comparison(result, result)["contracts_match"])

    def test_status_suite_has_a_separate_readonly_mixed_record_workload(self):
        with tempfile.TemporaryDirectory(dir=tempfile.gettempdir()) as directory:
            output = Path(directory) / "status.json"
            child = subprocess.run([sys.executable, str(Path(bench.__file__)), "run", "--suite", "status",
                                    "--runs", "2", "--warmups", "1", "--tasks", "2", "--output", str(output)],
                                   capture_output=True, text=True)
            self.assertEqual(child.returncode, 0, child.stderr)
            result = json.loads(output.read_text())
            self.assertEqual(result["suite"], bench.STATUS_SUITE)
            self.assertEqual(len(result["cases"]), 8)
            self.assertTrue(all(len(case["samples"]) == 2 and len(case["contracts"]) == 1 for case in result["cases"].values()))
            self.assertTrue(bench.comparison(result, result)["contracts_match"])


if __name__ == "__main__":
    unittest.main()
