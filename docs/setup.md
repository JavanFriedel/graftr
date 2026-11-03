# Setup

Part of the [Graftr guide](../README.md).

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
