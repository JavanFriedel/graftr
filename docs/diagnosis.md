# Diagnosis, environment refresh and inventory

Part of the [Graftr guide](../README.md).

## Worktree status

```bash
graftr status                         # this managed worktree, otherwise the full overview
graftr status fix-login               # exact checkout name, branch, path, or full ID
graftr status fix-login --project my-project
graftr status --all                   # full overview from any directory
graftr status --all --json            # explicit global scope for scripts
```

Inside a managed or adopted worktree, including nested directories, bare `status`
shows only that worktree, its setup receipts, addresses and any matching recovery
record. The focused heading identifies the task, branch and checkout path. Outside
one, the existing cross-project overview remains the default, including from a
project's main checkout or an unmanaged linked worktree. Selection never contacts
Herdr or probes applications; this is recorded state, not a live health check.

`status @` explicitly requests the current managed directory and fails outside
one. `--project` restricts target selection rather than listing a project's tasks;
use `list PROJECT` for that inventory. `--all` cannot combine with a target or
`--project`. Frozen tasks can also be selected by their recorded name/branch/path/ID.
Ambiguous targets are errors, never an arbitrary match.

Human and JSON output use the same scope. Focused JSON preserves the existing
task/setup/adopted/frozen arrays with only matching records and an empty `retained`
array. Use `status --all --json` to preserve the former cwd-independent global
behavior, or `history --json` for retained storage separately. Validation warnings
remain report-wide; damaged records are never silently treated as absent.

`graftr status` combines lifecycle and setup into one human `STATUS` column:

| Status | Meaning |
| --- | --- |
| `ready` | Setup completed with no cleanup/retirement problems recorded. Not proof that an application is running or healthy. |
| `setting up (STEP)` | The latest receipt records queued/running setup at that step. Worker liveness is not checked. |
| `setup incomplete (STEP)` | Setup failed, was interrupted or was cancelled before completion. |
| `setup skipped` / `setup unmanaged` | Setup was explicitly skipped or Graftr could not establish managed setup. |
| `setup unknown` / `setup needs review` | A setup receipt is missing or multiple receipts prevent a trustworthy result. |
| `closed` | Task recorded as closed; checkout and assigned ports retained. |
| `resources cleaned` | Configured resource cleanup finished; checkout retained. |
| `cleanup pending/queued/running/failed` | Resource cleanup is unfinished or failed. |
| `adopted — external setup` | Tracking only; setup remains externally managed. Selected-stage problems show `adopted — needs review`. |
| `freezing` / `frozen` / `thawing` / `attention` | Freeze/return lifecycle or manual recovery review. |

Problem qualifiers remain visible with inactive lifecycle labels, for example
`closed — setup incomplete (dependencies)` or `closed — cleanup failed`.
Retirement problems are likewise included. Combined labels use the most severe
color, so a closed task with failed cleanup stays red. Other setup records show
`setup completed`, not `ready`, because they do not establish managed ownership.
JSON and stored lifecycle/setup fields remain separate and unchanged.

## Incomplete status and inventory

`graftr status` and `graftr list PROJECT` isolate invalid individual task, setup,
adoption and (for status) frozen record files. Validated records remain visible;
the command identifies the affected paths relative to Graftr's state directory,
labels the report incomplete, and exits 1. It never prints rejected file contents,
trusts fields from them, repairs them, or releases their leases.

With `--json`, these reports retain their usable `data`, add `data.recordIssues`
(each with `path`, `code: "INVALID_RECORD"` and a generic `message`), and use
`ok: false`, `outcome: "partial"`, `error.code: "RECORDS_INCOMPLETE"`. Healthy output
is unchanged and has no `recordIssues` field. A damaged ownership record means an
otherwise unclaimed Git checkout has **unknown** tracking, not proven unmanaged;
non-main inventory entries also carry `incomplete-tracking` in their issues.

This is reporting-only tolerance, not lifecycle authorization. Allocation,
cleanup/removal, freeze/thaw and other strict readers still refuse invalid records.
Unsafe/unreadable category directories, configuration errors, Git inventory errors,
and retained-history failures still fail the report rather than pretending those
boundaries were checked. The separate `frozen` command remains strict. Review and
preserve named files privately before retrying; do not delete records merely to
make the warning disappear.

## Read-only task diagnosis

When a task stops working, start with recorded-resource diagnosis rather than
replaying setup or deleting anything:

```bash
graftr doctor fix-login
graftr doctor                           # local managed directory, including nested directories
graftr doctor my-project--fix-login --live --json
```

Local mode compares the canonical checkout, Git repository/registration/branch,
ownership records and port leases, current project profile, and last setup receipt.
It does not contact Herdr or Tailscale, even to infer a target. Supply an exact
name/branch/path/full ID, or run inside the managed directory; `--project` scopes
the same natural selectors used by other commands. A full ID can diagnose a missing
checkout. An absent/unreadable target record is an error, not permission to adopt it.

`--live` additionally reads workspace inventory from the task's **recorded Herdr
server**. Resources managed by hooks, such as Serve routes, are not inspected.
With `--live`, an omitted target can also use the explicitly owned caller workspace.
External reads have bounded transport/subprocess timeouts. An outage, malformed
response, or unavailable evidence is **unknown**, not proof of an absent workspace. The command never probes application listeners, HTTP pages, or certificates.

Each finding has `area`, `status`, `code`, `message`, and conditional `nextSteps`:

- `ok`: the evidence checked agrees; this is not an application health guarantee.
- `attention`: an observed conflict or incomplete setup needs review.
- `unknown`: evidence was unavailable or insufficient, including pending setup
  whose worker liveness cannot be inferred from its receipt.
- `not-checked`: live inspection was not requested.

The report contains `id`, `project`, `path`, `branch`, `mode`, overall `status`,
`complete`, and `findings`. Overall attention takes precedence over unknown;
`complete: false` still identifies incomplete evidence in a mixed report.
`complete` covers only requested observations: local success leaves both live
areas unchecked. Reports are independent point-in-time observations, not an atomic
snapshot; detected task-record changes invalidate confidence and request a rerun.

Exit 0 means all requested observations agree; exit 1 means attention/unknown
findings or an invalid request. `--json` uses the standard result envelope and
retains diagnostic failures in `data`, with `outcome: "failed"`, error code
`DOCTOR_ATTENTION` or `DOCTOR_INCOMPLETE`, and empty `progress`. This is not a partial
mutation. Target/state errors retain the standard error contract with null data.

Diagnosis writes no locks, records, receipts, leases, workspaces, or files.
It does not read `.env` files or execute installers, hooks, configured health checks,
or other project commands. Profile/receipt error contents and service responses are
not echoed in findings. A ready receipt is historical evidence, not current
environment/dependency validation. Existing `list`, `status`, completion, and
`reconcile` retain their existing responsibilities.

Reconciliation snapshots task/setup records under the registry lock, releases that
lock while reading Herdr, then rechecks the snapshot before recording any closures
or queuing cleanup. A concurrent record change invalidates the result and requests
a fresh explicit retry rather than overwriting newer state. This does not change
ownership checks or claim that every other command's external waits are lock-free.

Review findings and pending installer/hook effects **before** deliberately using
`setup`, `open`, or `cleanup`. Changed branches,
conflicting ownership, and interrupted removal require manual review; doctor never
repairs, adopts resources, releases leases, or automatically retries uncertain work.

## Run logs

Every installer run and every setup, cleanup and retire hook run writes its stdout and
stderr to a private log: `<state>/logs/<task-id>/<time>-<label>-<random>.log`, mode `0600` in
a `0700` directory. The child writes to the file directly, so exit and timeout handling are
unchanged. The log opens with the run's label and start time and ends with its outcome
(`exit N`, `timed out after N ms`, or `command could not start`).

```bash
graftr logs                      # list this task's logs, newest first
graftr logs fix-login --latest   # print the newest log
graftr logs my-project--fix-login --json
```

Output is never copied into CLI results, JSON, `status`, receipts or Herdr notifications.
Hook results (`hooks[].log`) and the setup receipt (`installLog`) record only the path, and
failure messages name it. Logs hold whatever those commands print, possibly including secrets,
so `--latest` prints only on request and is refused with `--json`. Each log is cut at 4 MiB
after its run, and each task keeps its 50 newest logs; older ones are deleted as new runs
start. Logs stay after a task is removed until [`graftr prune`](#pruning-old-state) deletes them.
Health-check commands still discard their output.

## Guided development environment refresh

`graftr env refresh [TARGET]` compares the configured development baseline with
an existing managed worktree's private `.env`. It is **preview-only by default**:

```bash
graftr env refresh fix-login
graftr env refresh fix-login --take-baseline EXAMPLE_SECRET
graftr env refresh fix-login --apply
graftr env refresh fix-login --take-baseline EXAMPLE_SECRET --apply --json
```

`TARGET` accepts the usual exact name/branch/path/full-ID selectors and `--project`.
Omit it (or use `@`) inside the managed checkout, including nested directories.
This command does not infer a target through Herdr and never contacts Herdr or
Tailscale. Only the explicitly requested command reads the private environment
files; `doctor`, inventory, status, and completion do not gain environment reads.
Completion does not read or suggest environment keys for `--take-baseline`.

There is no saved baseline history for existing worktrees. A differing value
could be a baseline rotation **or a deliberate local edit**; Graftr does not guess:

| Key status | Default action |
| --- | --- |
| `missing` | Add from the current baseline only when `--apply` is requested. |
| `different` | Preserve unless explicitly selected with `--take-baseline KEY`. |
| `same` | Preserve, unless explicitly selected to collapse duplicate declarations. |
| `local-only` | Preserve; absence from today's baseline is not proof of removal. |
| `managed` | Preserve fixed public assignments, including ports/URLs; selection is refused. |

`--take-baseline KEY` is repeatable, accepts **names only**, and is still read-only
without `--apply`. Unknown, duplicate, or managed key selections are refused.
Apply always includes missing baseline keys as well as selected differing or duplicate keys.
Without history, a missing key might be a deliberate local omission, not a newly
added baseline variable; review the preview before authorizing all missing additions.
No keys are removed, and no public assignments or profile changes are adopted.
Selected keys use the last baseline declaration's raw syntax, replacing the first
current occurrence in place and removing all other declarations of that key. A
selected key with duplicates is replaced even when its last value already matches.
Missing keys are appended. Untouched assignments (including unselected duplicates),
comments, quoting, BOM and line endings retain their bytes; the whole original file
is no longer necessarily a prefix. Values and shell references are never expanded
or executed, and no secret values are re-encoded. Application-specific interpolation
and externally inherited environment precedence still belong to the application's loader.

Both baseline and target must be stable private regular files owned by you at
canonical non-symlink paths, at most 4 MiB. The source must be the profile's approved
development file named `.env`, never `.env.prod`; the target must already exist and
be untracked. Refresh does not generate a missing environment or adopt a changed
checkout/branch. It conservatively accepts ordinary variable names, `export`,
`=`/colon assignments, comments, single/double/backtick quoting, and multiline values,
and refuses malformed, ambiguous, unsupported, binary, or invalid-UTF8 input rather
than silently dropping text. Fresh generation uses the same strict grammar; routine
setup still leaves existing files untouched and verifies only managed public values.

Pending setup, or preparing state without a recorded incomplete attempt, is refused
rather than assuming a worker stopped. A recorded failed attempt can be refreshed
without pretending its setup is ready. Apply takes the same per-checkout lock
nonblocking and the short registry lock, preserving setup/cleanup/removal ordering.
It rechecks checkout, task/profile/receipt identity,
baseline, target metadata and bytes before writing and again after backing up.
No installers, hooks, health checks, or application processes run.

### Recovery backups and interrupted apply

Every actual change first saves the **exact old target bytes** in a new mode-0600
file under the mode-0700 directory `<active-state>/env-backups/`, using a task-ID
prefix and unique suffix. The backup must be outside Git. Existing private-state
ownership and symlink checks apply; public/symlinked backup directories are refused.
Backup files are independent copies, not links to the mutable environment. No-op
apply creates no backup and does not rewrite `.env` (its explicit lock files may
still be created). Preview creates no files or locks.

**Backups contain secrets and survive `remove` and `nuke`.** They expire after 30 days
(see [Pruning old state](#pruning-old-state)); `graftr history` reports their count and age,
and `graftr prune` deletes them sooner when you ask.
No historical-baseline database, secret hashes, or automatic provenance snapshots
are introduced. Keep state and backups on private local storage, not in Git or a
shared/public directory.

After the backup is durable and inputs still agree, apply atomically replaces the
target with a private file. This is **not a transaction across the backup and target**.
A crash/failure may leave just a backup, or an already-replaced target. The optional
JSON envelope reports attempted `backup-environment` and `refresh-environment` stages
as completed/uncertain, never promises rollback, and returns failed/partial with
null data on errors. If a process dies before reporting, inspect the task-prefixed
private backups and current environment directly; no new recovery journal is assumed.

Stop external editors/writers before applying: Graftr's locks do not serialize
arbitrary writes, and its rechecks cannot eliminate an external write in the final
check/replace window. After an interrupted apply, preserve both current file and
backups, verify which contains the intended values **privately**, and restore only
the reviewed backup to the canonical untracked target with private permissions if
needed. Do not blindly replay an apply or copy a backup over a symlink/changed checkout.
There is no automatic rollback or force option.

Default output and `--json` expose key names/statuses/actions, **never values or
hashes**. The safe result contains `id`, `project`, `path`, `mode`, `status`
(`preview`, `applied`, `unchanged`), `entries`, `reviewRequired`, and nullable
`backupPath`. `reviewRequired` means differing values were deliberately preserved;
exit 0 means the requested preview/apply finished, not that every difference was
resolved. Entries describe that invocation's plan, not a fresh post-write inventory.
No task/setup/lease schemas are changed. Environment-only refresh does not install
dependencies, re-run setup hooks, or claim current application readiness.

**Existing shells and running applications keep their old environment.** Review
how your app loads dotenv files and deliberately restart/reload it as needed;
Graftr does not modify shell exports or restart processes.

## Inventory and orphan review

`graftr list PROJECT` compares `git worktree list --porcelain -z` with graftr's
records and the filesystem. It includes all of the repository's registered checkouts,
even existing numbered worktrees outside the configured task directory:

- **main:** the repository's main checkout.
- **managed:** a checkout with a graftr record; the listing includes task ID, recorded
  lifecycle state, local/secondary ports, and (in JSON) URL.
- **unmanaged:** Git knows the checkout, but graftr does not manage it. This is normal
  for your existing worktrees, not proof of an orphan.
- **unregistered:** a direct child of the configured worktree root has a `.git` marker
  but is not registered with this repository. Inspect it; it may belong elsewhere.

`REVIEW` marks missing checkout directories, records/checkouts without Git registration,
Git's prunable entries, branch mismatches, duplicate records, or conflicting repository
identity. Closing a task is not an issue: retained checkout and port assignments are
intentional. Ordinary directories without `.git` markers are not guessed to be worktrees.

Listing is read-only. It does not read `.env` values, require installed dependencies,
contact Herdr/Tailscale, inspect servers, adopt checkouts, prune Git metadata, release
leases, or remove files. Its state is a snapshot; rerun it after concurrent moves/setup.
It cannot discover an unregistered checkout moved to an arbitrary unknown directory.
`--json` returns `data.project`, `data.repo`, `data.worktreeRoot`, and `data.worktrees` with Git metadata,
`registered`/`present` flags, task summaries, last setup outcome, and issue codes.
`graftr status --json` returns full task records plus `setups` and `frozen` arrays.
Plain `graftr status` starts directly with an unindented worktree/setup/address table,
followed by any frozen return dates and recovery notes. It omits the command heading
and routine explanatory footer, keeping only `Use --json for full details.`
Long project/task names and addresses wrap within aligned columns before any fields
move below rows; very narrow terminals still use labeled fields. Values are not truncated.
STATE uses `prepared`, `closed` and `resources cleaned`;
unfinished cleanup shows its pending/running/failed state instead. SETUP uses `completed`
for a successful receipt, independently of whether the workspace was subsequently
closed. Missing or ambiguous receipts never imply setup completion. Completion follows
the configured policy: dependency installation may be explicitly skipped or not configured,
and servers are not started. JSON and persisted status values are unchanged.
Status does not scan retained history. Use `graftr history` separately for nonempty retained-storage categories with
entry counts, sizes and oldest dates, plus a total size and a pruning review hint.
This is not a chronological command log; counts do not mean every entry is safe to
delete. Headings share bold styling in terminals; `NO_COLOR=1` keeps them plain. Project and task
names occupy separate columns (or labeled fields on narrow screens), rather than
a combined `PROJECT--TASK` display key. Full IDs remain unchanged in JSON and
command targeting. The same distinction is used by frozen and due-return tables.
Status does not dump
registry fields, tab IDs or environment assignments. Listing only needs repository/root settings,
so invalid environment/install settings do not block inventory. Invalid records fail
validation instead of silently disappearing.

## Pruning old state

Graftr keeps a little short-lived history in private state: env refresh backups (which hold
secrets), migration backups, run logs, stale creation intents and cleanup scratch
directories, plus leftovers from earlier versions (records of removed tasks, timing samples,
and the original trial migration's `backups/`). None of it is needed for Graftr to work.

It expires by itself: at most once a day, the first lifecycle command (create, setup, open,
close, cleanup, remove, nuke, reconcile, or a Herdr event) deletes entries untouched for 30
days. That run never waits: if another operation holds the state lock, or anything fails, the
command carries on and a later command tries again. Read-only commands such as `status`,
`list`, `history`, `doctor` and `logs` never delete anything. `graftr history --json` reports
each category's `count`, `bytes` and `oldest` entry under `data.retained`.
`graftr status --json` retains the same metadata for existing scripts.
`graftr prune` clears things sooner:

```bash
graftr history                   # summarize retained logs, backups and other state
graftr prune                     # preview what is older than 30 days
graftr prune --older-than 7      # preview with a shorter cutoff
graftr prune --apply             # delete what the preview lists
```

Age is the newest modification inside an entry. Tailscale and Postgres hook receipts are
deleted only when no current task uses them and every route or database they record is gone
(route `none`, database `dropped`); otherwise the preview lists them as kept, with the reason.
Cleanup scratch directories still used by a current task are kept too. Task records, setup
receipts and lock files are never pruned, symlinks are never followed, and entries prune does
not recognize are listed and left alone. `--apply` plans again under the state lock before
deleting, and a rerun reports `unchanged`.
