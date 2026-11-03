# Setup

Part of the [Graftr guide](../README.md).

## Prebuilt release

The [release page](https://github.com/JavanFriedel/graftr/releases) provides Linux
x86_64 GNU archives built on Ubuntu 22.04. Other platforms are not currently
published or validated. Git and compatible Linux system libraries are required;
Rust, Bun, Node and Python are not needed to run a prebuilt release.

For a **new installation**, the following downloads the explicit v0.1.0 archive,
checks its reviewed SHA-256 before extraction, keeps the license notices alongside
the executable, and creates a non-overwriting launcher link:

```sh
(
  set -eu
  download=$(mktemp -d)
  trap 'rm -rf -- "$download"' EXIT
  cd "$download"
  archive=graftr-v0.1.0-x86_64-unknown-linux-gnu.tar.gz
  curl -fL --output "$archive" "https://github.com/JavanFriedel/graftr/releases/download/v0.1.0/$archive"
  printf '%s  %s\n' '07e09d4ddbd038aa7c10fbb0bc73f5d038524219f8f5a0be3fbf97e1ed25e998' "$archive" | sha256sum --check
  tar -xzf "$archive"
  destination="$HOME/.local/share/graftr/releases/0.1.0"
  test ! -e "$destination" && test ! -L "$destination"
  mkdir -p -- "$(dirname -- "$destination")" "$HOME/.local/bin"
  mv -T -- graftr-v0.1.0-x86_64-unknown-linux-gnu "$destination"
  ln -s -- "$destination/graftr" "$HOME/.local/bin/graftr"
) && graftr --help
```

Ensure `~/.local/bin` is on PATH. This uses curl, GNU tar/coreutils and a shell as
installation tools, not Graftr runtime dependencies. Existing destinations/launcher
links cause failure rather than silently replacing another installation. If linking
fails, the verified release directory remains available for inspection.
If an existing package manager or dotfiles setup owns `graftr`, use that owner's
upgrade process instead; do not overwrite its launcher.

For upgrades, review a new release and its checksums, install into a **new version
directory**, then deliberately switch your launcher. Keep older executable paths
needed by running workers and saved timers. Do not simply change the version in this
example without also reviewing/updating the checksum. The adjacent `SHA256SUMS` file
detects transfer corruption but is not an independent trust anchor.

### Version identification

Source builds now support `graftr --version` (alias `-V`) for a human-readable version,
or `graftr --version --json` for the standard JSON envelope with `data.version` and
`data.sourceRevision`. These commands neither read configuration/state nor invoke Git.
Release packaging embeds the exact source commit and checks the executable's report
before packaging. Ordinary source builds leave `sourceRevision` null rather than
guessing a commit from the directory where Graftr happens to run.

The published **v0.1.0 does not have this flag**. For that release, inspect
`RELEASE.json` in its archive/installation directory for version and source revision.
The flag will be included in the next release; existing releases are never overwritten.

## Native build and installation

Use `./build.sh` from a checkout to build, smoke-check and atomically publish
`dist/graftr`. Requires Rust/Cargo, a system linker/C toolchain and standard Linux
shell utilities. No Bun, Node, Python, JavaScript help exporter or external `flock`
is involved. Alternatively, `cargo install --path . --locked` installs the same
native binary into Cargo's installation root.

Only `graftr` and Git are needed for ordinary standalone lifecycle operations; the
binary also uses Linux system libraries. Put one intended version on PATH. A later
upgrade must replace the binary deliberately; checking out a different source version
does not update an already installed executable. Keep executable paths stable for
saved timer workers. Existing timers retain their absolute invocation until explicitly
rescheduled, so moving an installation is not just a PATH change.

User configuration and task state remain under HOME/XDG. Never copy another machine's
private registry/environment to provision a host. This source repository contains no
personal profiles, environments, state, logs or production worktrees.

## Optional Herdr plugin

Herdr 0.9.3+ must be on PATH; its server need not be running for installation:

```sh
graftr plugin install
herdr plugin list --json
```

This writes a private managed manifest under `$XDG_DATA_HOME/graftr/plugin` and runs
`herdr plugin link` explicitly. It does not start/restart Herdr, run startup hooks,
create timers or thaw anything during installation. The stable plugin ID is
`local.graftr`; its commands invoke `graftr` on the server's PATH. For development,
link this checkout's `plugins/herdr` directory instead of the managed copy.

The single startup worker reconciles owned workspaces and checks overdue returns
for that exact server. Set top-level `startupThaw: false` to disable only startup
returns; per-task timers remain separate. Event hooks handle creation, opening,
closure and native removal. They do not supervise application processes.

## Optional external services

Tailscale publishing is enabled only through configured hooks. Install/authenticate
Tailscale and review tailnet HTTPS separately. To permit unprivileged Serve changes,
the intended user may explicitly grant operator access:

```sh
sudo tailscale set --operator="$(id -un)"
```

That is a broad local Tailscale permission, not a single-route grant. Do not run it
as root or change another account's operator silently. Reading Serve status is not
proof that publishing is permitted. Retry setup deliberately after fixing permissions.

Postgres hooks require `psql`, `pg_dump`, the approved baseline `DATABASE_URL` and
appropriate database privileges. No database is required for Graftr's own state.
Consult [hooks](hooks.md) before enabling resource creation or retirement.
