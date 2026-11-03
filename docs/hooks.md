# Lifecycle hooks

Part of the [Graftr guide](../README.md).

## Unified lifecycle hooks

An approved project profile can add four narrow extension points without replacing
Graftr's lifecycle or removal handoff:

```json
"hooks": {
  "afterEnvironment": [
    { "name": "local-config", "command": ["sh", "scripts/prepare-local.sh"] }
  ],
  "afterDependencies": [
    { "name": "generate", "command": ["pnpm", "run", "generate"], "timeoutMs": 120000 }
  ],
  "cleanup": [
    { "name": "report-closed", "command": ["sh", "-c", "echo \"$GRAFTR_TASK_ID closed\""] }
  ],
  "retire": [
    { "name": "report-removed", "command": ["sh", "-c", "echo \"$GRAFTR_TASK_ID removed\""] }
  ]
}
```

These are example commands for scripts you provide and explicitly approve, not files
Graftr discovers or generates. All phases are optional, with up to 16 commands per
phase. Array order is execution order; names must be unique within a phase and use
the usual lowercase-letter/number/single-hyphen naming rules (48-character limit).
Only `name`, `command`, and optional `timeoutMs` are accepted. Commands are nonempty
literal argv arrays (up to 64 arguments, no control characters); strings/background
flags/unknown phases are rejected. `timeoutMs` defaults to `60000`, with an integer
range of `100`–`600000`. There is no interpolation in argv, implicit shell, glob,
or environment expansion; explicitly name a shell/script if that trusted code needs one.
`["sh", "-c", "your-command --port \"$GRAFTR_LOCAL_PORT\""]` needs no helper file.
The retired `setupHooks` key is converted to `hooks` by `graftr migrate`.

The pipeline is environment → **afterEnvironment hooks** → shell tabs → dependencies
→ **afterDependencies hooks** → final ownership/environment checks → readiness. The first phase must
not assume dependencies have been installed in a cold checkout. The second phase runs
even for skipped/manual dependency preparation; commands can check the provided status.
Hook changes alone do not invalidate a valid installer fingerprint, but changes to
actual dependency inputs/outputs do. Hooks cannot leave a successful installation
receipt for changed inputs or missing required outputs.

Setup hook cwd is the recorded canonical checkout. The process inherits the caller environment
plus the public task overrides and these context variables (which take precedence over
inherited or configured `GRAFTR_*` assignments):

| Context variable | Value |
| --- | --- |
| `GRAFTR_PROJECT` | Profile name. |
| `GRAFTR_TASK_ID` | Recorded managed task ID. |
| `GRAFTR_WORKTREE_PATH` | Recorded absolute checkout path. |
| `GRAFTR_BRANCH` | Recorded attached branch. |
| `GRAFTR_LOCAL_PORT`, `GRAFTR_HTTPS_PORT`, `GRAFTR_URL` | Assigned public addresses; optional HTTPS/URL variables are omitted when unconfigured, clearing stale inherited values. |
| `GRAFTR_PORT_<NAME>` | Each named port, with the name uppercased and hyphens as underscores (`ws-proxy` → `GRAFTR_PORT_WS_PROXY`); inherited `GRAFTR_PORT_*` values are always cleared. |
| `GRAFTR_HOOK_PHASE`, `GRAFTR_HOOK_NAME` | Current phase and hook name. |
| `GRAFTR_DEPENDENCIES` | `pending` before dependency preparation; after it, `ready`, `skipped`, or `not-configured`. |
| `GRAFTR_SETUP_RUN_ID` | Current setup request's fencing ID, not an authentication credential. |
| `GRAFTR_TASK_SLUG` | Resource-name-safe identifier, `<project>_<checkout folder>` lowercased with other characters as `_` (at most 48 characters); also `{{taskSlug}}` in `envOverrides`. |
| `GRAFTR_RETIRE_RUN_ID` | When a retire plan is recorded: its ID, stable for the task's whole lifetime across close/reopen generations. Name per-task resources with it. |
| `GRAFTR_CLEANUP_RUN_ID` | When cleanup is configured: stable provisioning-generation ID during setup and cleanup, also supplied as `GRAFTR_SETUP_RUN_ID` during cleanup; not the setup request ID or retry attempt counter. |

Graftr does not source the private `.env` into the child process. Approved scripts
can read it themselves if necessary. The profile and everything its commands invoke
(including branch-controlled scripts) are trusted code running as your user, **not a
sandbox, per-commit approval system, or automatic repository-hook discovery**. Do not
use setup hooks to launch servers, background daemons, or replace lifecycle operations.

Commands are sequential under the per-checkout setup lock, **not the shared registry
lock**. Other checkouts remain responsive; cleanup fences setup then waits for the
checkout-operation lock, while CLI removal refuses active setup. Stdin is closed. Each run's
stdout/stderr go to a private run log (see [Run logs](diagnosis.md#run-logs)), so script output
is never copied into CLI output, receipts, or Herdr notifications; hook results record only the
log's path. Timeouts terminate only the
newly owned subprocess group, and the runner cleans up same-group descendants when the
command exits. Descendants that deliberately detach into another group are not contained.
Commands must be finite and idempotent. They run again on a new setup/open/retry attempt;
resuming after worker interruption may rerun earlier commands. There is no exactly-once
guarantee or hook-success cache. Duplicate live workers for the same request do not
duplicate successful setup execution.

Hook failure stops later hooks/readiness, keeps the checkout/records/leases for review,
and reports `step: "hooks"` with optional `hooks` observations in setup receipts and
JSON `data.hooks`. Each observation has phase, name, `running`/`succeeded`/`failed`, and
available elapsed time, safe failure reason and exit code—never argv or output. Setup
timings include a hooks stage; JSON progress includes `run-hook` attempts. These are
additive optional receipt fields/stage values; old receipts without hooks remain valid.
`status: incomplete` plus a `running` hook means that was its **last observation**, not
proof the command is still alive after interruption/cancellation.

Run IDs and ownership are checked before/after commands and before later stages. A hook
already active during cleanup or supersession may finish its own effects, but its stale
worker cannot overwrite the new receipt, execute remaining hooks, or publish success. Profile, canonical repository/branch, managed environment and required
dependency state are rechecked; changing those from a hook cannot quietly publish ready.
No setup hooks run for `--no-setup` or unmanaged fallback.
Cleanup hooks do run during cleanup/close/remove/nuke/reconcile and native post-events.
Native enrolled worktrees use the same pipeline as CLI creation.

There is no automatic rollback of arbitrary script effects and no new cleanup/force
bypass. A failure while reopening may leave a previously published resource unchanged; it
does not claim a fresh ready setup. Inspect status, fix the reported issue, then retry
`graftr setup --wait` inside the owned Herdr worktree. Use `graftr help hooks` for a
short command-line reference. No hook is enabled in your profiles automatically.

## Durable cleanup and native deletion

`retire` hooks are for resources that must outlive close/reopen but not the task, such
as a per-task database. Their plan is recorded once, at the first setup that finds
retire hooks in the profile, with the same private argv/context as cleanup plans, and
later profile edits never retarget it. It runs only after CLI `remove`/`nuke` or a
verified native removal has deleted the checkout, outside the registry lock and before
the record is deleted. A failed retire hook keeps the record and leases with the
checkout already gone; automatic events never replay it, and an explicit
`graftr remove` retry finishes retirement.

Before provisioning, Graftr privately records cleanup argv and the public task environment
with a generation ID. An armed plan cannot be removed/retargeted by profile edits or
changed public overrides. The plan survives deletion of the checkout and uses its
recorded commands/context, not a newly edited profile or deleted script. Cleanup runs
in a private scratch directory outside the checkout. Its executable must be absolute
or a PATH name; use inline `sh -c` or an independently installed executable, not a
checkout-relative script. `GRAFTR_WORKTREE_PATH` is historical and may already be gone.
Inherited PATH/environment are not snapshotted; keep cleanup tools available to workers.

Cleanup/close fence setup before waiting for the per-checkout operation lock, outside
the registry lock. This lets a publishing command already in progress finish before
its matching cleanup, without letting stale setup publish readiness or run later stages.
Arbitrary cleanup commands also run outside the registry lock. CLI remove/nuke keep
their ownership, Git and confirmation preflights, run cleanup before deletion, reverify,
and release leases only after successful cleanup. They do not gain a force bypass.

Native Herdr close/removal is still Herdr's own operation. Post-events wait for active
setup and run the recorded cleanup even when the checkout was deleted. Failure cannot
undo native deletion: it reports incomplete auxiliary cleanup and retains ownership/
leases for review. A worker discovering a missed close fences setup and queues cleanup;
explicit `cleanup` or `reconcile` can drain it outside the worker's registry lock.
`reconcile` still skips standalone tasks and does not retire absent checkouts itself.

Results persist per command. Successful commands are skipped on retry and duplicate
events do not rerun a completed generation. Failed/interrupted commands are **not**
blindly replayed by automatic events or a stale concurrent waiter; inspect their effects
and deliberately retry `graftr cleanup`, `close`, `remove` or `nuke` as appropriate.
Commands must be finite/idempotent because interruption can leave external effects
without a success record. Once cleanup completes, a new setup/open arms a fresh plan;
outstanding failed/queued/running cleanup blocks reprovisioning. Setup retries within an
armed generation may rerun setup hooks, so publishing must also be idempotent. Those
retries preserve `GRAFTR_CLEANUP_RUN_ID`; `GRAFTR_SETUP_RUN_ID` still changes to fence
each setup request and must not be used as a stable external resource identity.

Status shows cleanup generation, attempt counter, safe names/timeouts/results and
status (`armed`, `queued`, `running`, `failed`, `complete`), not argv or recorded environment
values. Internal records contain these public command/context values and remain private;
status is not a lossless registry backup. Failure retains the task/leases, and cleanup
must complete before the record is deleted. This is a receipt/fencing protocol, not an exactly-once
external transaction or a universal verifier of resources created by arbitrary commands.
No pre-existing app/server process is signaled and no daemon is introduced.

## Per-task Postgres databases

`graftr hook postgres` gives each task its own copy of the development database and
drops it when the task is removed. Add it to both phases, and raise `timeoutMs` for a
large database (the default is 60 seconds, the maximum 600000 ms):

```json
"hooks": {
  "afterEnvironment": [
    { "name": "database", "command": ["graftr", "hook", "postgres"], "timeoutMs": 600000 }
  ],
  "retire": [
    { "name": "drop-database", "command": ["graftr", "hook", "postgres"] }
  ]
}
```

In `afterEnvironment` the hook reads `DATABASE_URL` from the profile's `envSource`
baseline (a `postgres://` or `postgresql://` URL), creates `<database>_<taskSlug>`
(shortened with a hash past Postgres's 63-byte limit), copies the development database
into it with `pg_dump | psql`, and replaces `DATABASE_URL` and `POSTGRES_DB` in the
worktree's private `.env` with one assignment each for the task's database. Existing
occurrences are replaced in place and duplicates removed; missing keys are appended.
Unrelated raw syntax is preserved, not expanded or executed. Unsupported environment
syntax is refused before database creation/copy. The password travels only through `PGPASSWORD`,
never in argv, records or output. It refuses to adopt an existing database with the
task's name, and `envOverrides` must not also manage `DATABASE_URL` or `POSTGRES_DB`.
Retries are idempotent; a partial or uncertain copy is dropped and recreated.

Its private receipt under the state directory's `postgres-hooks/` names the server,
user, source and task database (never the password) and records a `creating` intent
before `CREATE DATABASE`. `retire` drops only that database, using the baseline's
credentials, and refuses when the baseline now names a different server, user or
database. It uses plain `DROP DATABASE`, so a database still in use (for example by a
running dev server) is kept and the retire hook fails until you stop it and retry
`graftr remove`. Close and cleanup keep the database; reopening reuses it.

`psql` and `pg_dump` must be on the hook's PATH and compatible with the server. Redis
and other services stay shared. `graftr env refresh` reports the task's
`DATABASE_URL`/`POSTGRES_DB` as `different` from the baseline by design: keep them
(the default), since `--take-baseline` would point the task back at the shared database.
