# Upgrading: migration and the Devtree rename

Part of the [Graftr guide](../README.md).

## Migrating older records and profiles

Earlier Graftr versions published Tailscale routes from core. Task records from those
versions (schema 2 or 3) and profiles using `originsKey`, `setupHooks`, or `httpsPorts`
without `url`/`hooks` are now refused with a message to run `graftr migrate`:

```bash
graftr migrate           # preview; changes nothing
graftr migrate --apply   # back up, then convert
```

Applying a migration:

- gives each legacy profile `url: https://<host>:{{httpsPort}}` and the two Tailscale
  hooks, spells out its former default `envOverrides` (`PORT`, `BASE_URL`, `AUTH_URL`
  and the `originsKey` variable) when it had none, and drops `originsKey`;
- renames `setupHooks` to `hooks`, and rewrites older `bun -e … tailscaleHook` commands
  to `graftr hook tailscale`;
- converts every record to schema 4 with an explicit `envOverrides` map;
- hands each published or pending route to an armed cleanup plan plus a route receipt,
  so the next cleanup, close or remove unpublishes exactly that route;
- finally renames config/state directories still named `devtree` to `graftr` (see
  [Rename from Devtree](#rename-from-devtree)).

The host comes from the project's existing records; Tailscale is asked only when a
project has none. Unfinished legacy cleanup, or records with more than one host in a
project, block the whole apply without changing anything. Apply runs under the state
lock and first copies the configuration and every record to
`<state>/migration-backups/<timestamp>/`. It writes receipts, then records, then the
configuration, so rerunning it finishes an interrupted apply; a migrated machine
reports `unchanged`. It does not touch Serve, `.env` files, ports, workspaces or
checkouts.

Upgrade every executable/alias and Herdr worker sharing the state directory before
applying: older binaries cannot read schema 4 records. Address contracts stay pinned:
use a fresh task for a different port/URL contract. A recorded outstanding cleanup plan
cannot be retargeted through profile or public-environment edits: finish old cleanup
first, then deliberately update the environment and reprovision.

## Rename from Devtree

Graftr was formerly Devtree. The `devtree` launcher, its completion alias and the
`tools/devtree`/`plugins/devtree` folder links are retired: use `graftr`. The Herdr
plugin's ID remains `local.graftr` for compatibility and its commands run `graftr`. After upgrading Graftr,
check `herdr plugin list`. A plugin first linked through `plugins/devtree` still shows
`local.devtree` and falls back to an old manifest snapshot whose commands no longer
exist, so native close/remove events fail until it is relinked:

```bash
herdr plugin unlink local.devtree
herdr plugin link ~/graftr/plugins/herdr
```

A plugin linked through `plugins/graftr` already shows `local.graftr` and needs nothing.
No plugin config needs carrying over. A `~/.local/bin/devtree` or `devtree` completion
link left from an earlier deployment then points at nothing and can be deleted.

Paths are `${XDG_CONFIG_HOME:-~/.config}/graftr/projects.json` and
`${XDG_STATE_HOME:-~/.local/state}/graftr`. A machine still using devtree config or
state directories keeps working: Graftr uses them while the graftr ones are absent,
and refuses to run with two independent state directories rather than split records
and leases. `graftr migrate --apply` renames them last, after converting records, under
the state lock. The rename keeps every file and lock inode, and a `devtree → graftr`
symlink stays behind for processes that resolved the old path. It refuses when the
graftr destination already exists. Verify with `graftr status` afterward.

## Standalone task/receipt compatibility

Existing task records remain readable. New unattached tasks use `socket: null`,
`workspaceId: null` and no tabs; IDs, paths, branches, environments and leases do not
change on attachment. Records are task schema 4 (see **Migrating older records and
profiles**). Do not run old and new builds against the same active state.

Standalone and shared enrollment create receipts with a path-based core key (null
socket, schema version 1). The integrated CLI creation acknowledgement still uses its
compatible socket-scoped key. Existing socket-scoped receipt files retain their
original filename and lineage. That stored
socket is a compatibility storage context, not the current adapter address: the task's
attachment identifies the server. A verified new attachment can resolve the same legacy
receipt. Multiple receipt lineages for one path are refused, not merged automatically.
There is no bulk rewrite, registry split, or migration of private files.
