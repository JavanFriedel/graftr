# Graftr

Native Git worktree setup, cleanup and scheduled return. Give each task its own
checkout and development environment; optionally attach Herdr or publish a private URL.

Graftr is a **one-shot tool, not a server supervisor**. You start your applications.
Human-readable output is the default; use `--json` for structured results and failures.

## Install

Prebuilt **Linux x86_64 GNU** binaries are available on the
[releases page](https://github.com/JavanFriedel/graftr/releases).
[Installation instructions](docs/setup.md#prebuilt-release) pin an exact version and
verify its archive checksum before running it. No Rust, Bun or Python is needed.
Git and Linux system libraries are still runtime requirements; integrations have
their own optional dependencies below.

## Build from source

Supported platform: **Linux**. Requires Rust/Cargo and the system C linker/toolchain
to build. No Bun, Node, Python or external `flock` is required to build or run Graftr.

```sh
git clone https://github.com/JavanFriedel/graftr.git
cd graftr
./build.sh
./dist/graftr --help
```

The locked release build smoke-checks a staged binary and atomically publishes
`dist/graftr`. Failed builds keep the previous binary. `CARGO_TARGET_DIR` selects the
build cache; relative paths resolve from this repository. `CARGO_NET_OFFLINE=true`
prohibits downloads when the locked crates are already cached. The builder selects
the native host target; other operating systems and cross-target releases are not
currently validated. The executable dynamically links Linux system libraries.

You can also use `cargo install --path . --locked` to install into Cargo's bin
directory. Put your chosen installation on PATH as `graftr`; Herdr workers and hooks
must resolve that same command. Future release upgrades should select explicit
versions, not fetch "latest" on every invocation. No release tag is implied by cloning.

## First task

Create `~/.config/graftr/projects.json` (XDG overrides are supported):

```json
{
  "schemaVersion": 2,
  "herdr": "off",
  "projects": {
    "my-project": {
      "repo": "/absolute/path/to/repository",
      "worktreeRoot": "/absolute/path/to/worktrees",
      "localPorts": [3200, 3300],
      "envOverrides": { "PORT": "{{localPort}}" }
    }
  }
}
```

```sh
graftr create my-project fix-login
graftr status
graftr close my-project--fix-login
graftr open my-project--fix-login
graftr freeze my-project--fix-login --note "Return after review"
graftr thaw my-project--fix-login
```

`close` keeps the checkout and branch. `remove` deletes a clean checkout and its
ignored files but keeps the branch. `nuke` additionally deletes a verified merged
local branch, with typed terminal confirmation. `freeze` reclaims the checkout's
disk space and retains recovery metadata; `thaw` regenerates setup, not ignored files.
Dirty work is not silently stashed, discarded or committed.

## Dependencies and integrations

- **Git** for worktree and branch operations.
- **Herdr 0.9.3+**, optional, for workspaces, shell tabs and independent plugin workers.
  Set `herdr` to `auto` or `required`, then run `graftr plugin install` explicitly.
- **systemd user timers**, optional, for persistent per-task one-off scheduled returns.
- **Tailscale**, only for a configured `graftr hook tailscale` publisher.
- **psql/pg_dump**, only for configured per-task Postgres hooks.
- Any installer or hook executables you explicitly configure.

There is no background Graftr daemon, database, runtime JavaScript or automatic
TypeScript fallback. Health probes run only when requested. Hooks are trusted commands,
not a sandbox; review them before enabling them. See [security boundaries](SECURITY.md).

## Documentation

| Guide | Covers |
| --- | --- |
| [Setup](docs/setup.md) | Installation, dependencies and optional plugin |
| [Everyday use](docs/everyday-use.md) | Creation, reopening, targets and completion |
| [Configuration](docs/configuration.md) | Profiles, naming, environments and hooks |
| [Adoption](docs/adopting.md) | Preserve-first tracking of existing worktrees |
| [Diagnosis](docs/diagnosis.md) | Status, doctor, logs, health and environment refresh |
| [Removal](docs/removal.md) | Safe removal and merged-branch deletion |
| [Freeze/return](docs/freezing.md) | Storage, persistent timers and retry policy |
| [Networking](docs/networking.md) | Ports, public origins and optional publishing |
| [Hooks](docs/hooks.md) | Setup, cleanup, retirement and optional databases |
| [Scripting](docs/scripting.md) | JSON envelopes, partial failures and recovery |
| [Upgrading](docs/upgrading.md) | Legacy record/config compatibility |
| [Benchmarks](docs/benchmarks.md) | Repeatable timing, RSS and lifecycle contracts |
| [Releases](docs/releasing.md) | Versioned packaging, checksums and deliberate publication |

`graftr help getting-started` and `graftr <command> --help` also work without configuration.
Source builds now support `graftr --version` (or `-V`) and `graftr --version --json`
without configuration or Git access. Release packaging embeds and verifies its source
revision; ordinary source builds report the package version with no revision claim.
The already-published v0.1.0 predates this flag; use its `RELEASE.json` for identification.

## Check

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
./build.sh
python3 -m unittest discover -s tests/cli -p 'test_*.py'
python3 -m unittest discover -s benchmarks -p 'test_*.py'
python3 -m unittest discover -s tests/build -p 'test_*.py'
python3 tests/check-native-build.py
```

Python 3.10+, Git and GNU `time` are test/benchmark tools, not build/runtime dependencies.
Tests use private disposable repositories/HOME/XDG state and fake external integration
boundaries, never your current Herdr server, systemd manager, database or private profiles.
238 checked-in help snapshots cover every command/topic at seven widths. Review changes
before regenerating snapshots with `python3 scripts/update-help-snapshots.py`.

The CLI regression suite retains the port's behavior assertions. By default, its
comparison helpers check native repeatability, **not independent feature parity**.
Set `GRAFTR_REFERENCE` to a nonempty JSON argv array for an explicitly supplied other
implementation/version; no legacy source is bundled or automatically fetched. Older
reference help/documentation may intentionally differ. `GRAFTR_NATIVE` selects the
candidate binary; it defaults to this checkout's `dist/graftr`.

## License

[MIT](LICENSE). This repository starts with a fresh history. Machine configuration,
private environments, task registries, logs and historical research are not included.
