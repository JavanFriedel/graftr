# Benchmarks

Part of the [Graftr guide](../README.md).

The language-neutral Python runner measures fresh native CLI processes in private
temporary directories using GNU `time`. The standard suite contains 17 cases over
real disposable worktrees: inspection/completion, creation, cached/forced setup,
opening, cleanup/close, freeze/thaw, dirty refusal and clean removal.

```sh
./build.sh
python3 benchmarks/bench.py run --output /tmp/graftr-before.json
python3 benchmarks/bench.py run --label candidate \
  --command '["/absolute/path/to/another/graftr"]' --output /tmp/graftr-after.json
python3 benchmarks/bench.py compare /tmp/graftr-before.json /tmp/graftr-after.json
```

Defaults: 25 populated tasks, 2 warmups, 10 measured repetitions. Existing output
files are never overwritten. `--suite history` and `--suite status` are smaller,
separate recorded-state contracts; comparisons reject unlike suites/workloads.

Wall time includes process launch and the meter. CPU includes waited-for children.
RSS is the maximum individual-process high-water mark, **not aggregate process-tree
memory**. Filesystem caches are warm/uncontrolled, not reboot/cold-cache measurements.
The instrumentation floor is recorded but never subtracted. Saved results can contain
hostnames and local executable paths: review them before sharing publicly.

Normalized output contracts preserve statuses, ordering, errors and meaningful data;
only documented volatile fixture paths, timestamps, IDs, timings and fingerprints
are masked. Read-only commands are checked for state mutation. Synthetic installers
write local markers; service executables are failing traps. No network downloads,
real Herdr session, database, route or timer is used. This is not a sandbox: only
benchmark trusted executables. It supplements, rather than replaces, regression tests.
