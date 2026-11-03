# Adopt existing worktrees

Part of the [Graftr guide](../README.md).

Use adoption to track a linked Git worktree whose development setup already exists:

```sh
graftr adopt /path/to/existing/worktree
# Or, from inside that checkout:
graftr adopt
```

The repository must match one configured project. Adoption keeps its current path
and attached branch; the main checkout, detached/locked/prunable worktrees,
symlinked paths, conflicting ownership and frozen reservations are refused.
Uncommitted work is allowed: no reset, stash, commit, checkout recreation or copying
occurs. Checkouts outside `worktreeRoot` can be adopted.

## Defaults preserve existing setup

Default adoption writes only private tracking/lock files. It does **not** allocate
ports, change `.env` or its permissions, install dependencies, run hooks, start a
server, configure Tailscale or create/rename/focus a Herdr workspace. When available,
it records an existing attachment on the contextual Herdr server.

Read-only environment observation accepts bounded, canonical, owner-written regular
root `.env` files (including 0644). Only a literal numeric `PORT` and credential-free
HTTP(S) origin from `BASE_URL` are recorded; no other values or secret hashes are
saved. Unsafe/missing files and nonliteral assignments leave the address unrecorded,
not guessed. Observation is not environment validation. Known ports are reserved
against future Graftr allocations; no listener needs to be stopped or available.

`graftr status` shows **adopted / external** alongside provisioned tasks.
`graftr list PROJECT` distinguishes adopted from unmanaged checkouts. Full IDs are
derived from the folder name, with a stable path suffix when normalization is needed.
Repeating default adoption is a no-op, including after a selected stage failed.

Adoption is deliberately distinct from provisioned service ownership. Existing
manual routes/databases remain external. Automatic event setup and Graftr's
open/close/cleanup/remove/nuke/freeze/health/label operations are not enabled for
these records. Use Herdr/Git directly for their workspace/checkout lifecycle.
`logs` and `doctor` accept adopted IDs and paths; doctor reports setup as unverified,
not successfully provisioned. Moving/changing branches requires explicit review;
repeating adoption does not silently rebind the old record.

## Explicit, independent setup stages

Each invocation selects only what to run:

```sh
graftr adopt . --install
graftr adopt . --hook afterDependencies/check
graftr adopt . --install --hook afterEnvironment/prepare --hook afterDependencies/check
```

Hooks use the project's configured argv/timeouts; names alone are accepted when
unambiguous. Order is environment refresh, selected `afterEnvironment` hooks,
installation, then selected `afterDependencies` hooks. No cleanup or retire hooks
are armed. Built-in Tailscale/Postgres publishers are refused because they require
provisioned ownership; adoption cannot pretend to own a manual resource.

Installers and selected custom hooks are trusted commands and may have arbitrary
effects. They run synchronously under the per-checkout lock, not the global state
lock. Existing `.env` files are not sourced/exported into the parent process.
Hook context includes known public addressing only; unknown ports remain unset.
Output goes to private run logs, never the result or status. Stage results are
recorded as pending/completed/failed; a failure or interruption leaves registration
and evidence intact. Inspect the worker/effects before explicitly retrying. Default
adoption and startup events never replay a selected stage.

## Review environment changes first

```sh
graftr adopt . --environment
graftr adopt . --environment --apply
graftr adopt . --environment --take-baseline EXAMPLE_KEY --apply
```

`--environment` without `--apply` is entirely read-only, including registration and
locks. Preview cannot be combined with installation or hooks. Refresh requires an
existing private, untracked, canonical `.env` and a separate approved `.env` baseline;
both must be stable regular files owned by you, at most 4 MiB. Missing/unsafe files
are not repaired. Public port/URL keys and declared assignment keys are protected;
adoption will not replace them or invent missing address assignments.

Apply adds missing non-address keys and preserves existing differences unless
explicitly selected with `--take-baseline`. Selected keys are replaced in place
using the last baseline declaration's raw syntax, collapsing all their duplicates
even when the last value matches. Unselected bytes and duplicate keys are preserved;
no keys are removed or references expanded. Exact original
bytes are backed up privately outside Git, then the files and configuration are
rechecked before replacement. Only key names/statuses/actions appear in reports.
Backups contain secrets and use the existing retention/pruning policy. Stop external
writers before applying; locks serialize Graftr, not arbitrary editors. No automatic
rollback, process restart or shell environment refresh occurs.

## Stored state and JSON

Schema-1 records live in `$XDG_STATE_HOME/graftr/adopted/` (normally
`~/.local/state/graftr/adopted/`), separately from provisioned schema-4 task records.
Older clients cannot mistake adopted setup for cleanup ownership. Prune never
expires these live tracking records. Existing task/setup schemas are unchanged.

Use `--json` for the normal versioned result envelope. `adopt` returns `data.status`,
`data.worktree` and optional environment key/action metadata. `status --json` adds
`data.adopted`; existing `data.tasks` keeps its provisioned-task meaning. Inventory
uses `tracking: "adopted"` with an `adoption` record. Stage writes appear as
`save-adoption`, `install-dependencies`, `run-hook`, `backup-environment` and
`refresh-environment` progress. A failed invocation can be partial: registration,
logs, backups or external command effects may already exist.
