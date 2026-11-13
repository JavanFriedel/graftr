# Scripting: JSON results and partial failures

Part of the [Graftr guide](../README.md).

## Machine-readable command results

`transfer --dry-run` uses the same envelope with `data.dryRun: true`,
`data.canTransfer`, `data.findings` and `data.status: "checked"` or `"attention"`.
Read-only failure exits 1 with `outcome: "failed"`; success is preflight only, not
destination setup readiness. Findings identify checks not performed.
Missing projects additionally return `data.projectMigration` (portable field names,
destination-local suggestions, blockers and approval hash) and `data.approvalCommand`.
No raw source command argv or environment values are included in the proposal.
`--approve-project SHA256` explicitly authorizes only that current missing-project
proposal. `--destination-config PATH` refers to a private profile fragment on the
destination, never an existing-project overwrite. These options cannot combine
with `--retry-hooks`; approval cannot combine with `--dry-run`.
See [transfer preflight and migration](transferring.md#dry-run-before-sending).

Public commands are human-readable by default, including create/setup/open,
cleanup/removal, health, labels, freeze/thaw/scheduling and plugin installation.
Scripts and agents must explicitly request `--json`; output is never switched to
JSON automatically when redirected. Envelope versions and safety/confirmation
checks are unchanged; incomplete status/list reports follow the contract below.

`status --json` now follows the same directory-sensitive scope as human output:
inside a managed/adopted worktree it includes only that worktree, otherwise all.
Use `status --all --json` for a cwd-independent global report, or
`status TARGET --json` for an explicit worktree. Focused output keeps the existing arrays but
leaves `data.retained` empty and omits unrelated records. Record validation warnings
remain report-wide; selection does not contact Herdr.

Bare `close`/`remove` (or target `@`, without `--project`) in an unmanaged Herdr
workspace require interactive confirmation for a close-only alternative, even
with `remove --yes` or `--json`. Noninteractive calls return
`CONFIRMATION_REQUIRED`. Confirmed JSON results use `data.action: "close-workspace"`,
`data.managed: false`, and the workspace identity rather than a task ID. `remove`
does not mean files were deleted in this case. `queued` means the independent
worker was accepted; inspect Herdr/plugin logs, not task status. No task is enrolled.

Human tables use the terminal width: status first wraps long project/task names and
addresses inside their columns, keeping Address in the table when readable columns fit.
On narrower screens, long trailing columns move beneath each row,
and narrow terminals get stacked labeled fields. Paths, identifiers, URLs and notes
are wrapped without truncation. Redirected output defaults to 80 columns; set
`COLUMNS` to choose another width. `--json` and raw `logs --latest` are never wrapped.
Internal event/startup/timer workers retain their machine-readable protocol.

`graftr adopt PATH --json` registers existing external setup without provisioning by
default. `status --json` adds `data.adopted` alongside unchanged provisioned tasks;
inventory uses `tracking: "adopted"` and an `adoption` record. Optional stage progress
includes `save-adoption`. See [Adopt existing worktrees](adopting.md) for explicit
installer/environment/hook controls and the external-ownership boundary.

Help uses one command/description alignment across groups and one argument/option
alignment per command page. Wrapped descriptions keep a hanging indent; very narrow
terminals place descriptions beneath their labels instead.

Interactive human output colors semantic statuses: ready/thawed/healthy in green,
closed/pending/unknown in yellow, frozen in blue, failures in red, and skipped or
inactive states in gray. Status words remain present; color is never the only signal.
Colors are disabled for redirected output, `TERM=dumb`, or when `NO_COLOR` is set
(for example `NO_COLOR=1 graftr status`). JSON and internal worker output never
contain generated terminal colors. Coloring does not affect table widths or wrapping.
Headings share bold styling. Global `status` starts directly with its unindented
table; focused status first identifies the task, branch and checkout path.
The `--json` hint remains; focused status also points to `--all`.
The human `status` table combines lifecycle and setup in one `STATUS` column: `ready`,
`setting up (STEP)`, `setup incomplete (STEP)`, `closed`, `resources cleaned`, or
`adopted — external setup`. Missing or conflicting receipts show `setup unknown`
or `setup needs review`. Cleanup/retirement problems remain visible, including
on closed tasks; failures take color precedence over lifecycle labels.
`ready` means recorded Graftr setup completed, not that an app is running or healthy.
JSON and stored state keep their separate existing values and schemas.
`status --json` omits recorded hook commands and context from both cleanup and
retirement plans. Hook names, timeouts, state, attempts and results remain visible;
private persisted plans are unchanged.
`graftr history` separately shows
retained storage as a category/count/size/oldest-date table. Its `--json` result has
`data.retained`; global `status --all --json` retains the same metadata.
Maintenance commands (`env refresh`, `prune`, `migrate`) use yellow
for previews or differences, green for applied results, and red for blocked changes.
Environment refresh still exposes key names/actions only, never private values.

Lifecycle and inspection commands support `--json` for one result envelope on stdout:

```bash
graftr create my-project fix-login --wait --json
graftr open fix-login --json
graftr list my-project --json
graftr status --json
graftr history --json
graftr help output
```

The envelope always contains `schemaVersion: 1`, `command`, `ok`, `outcome`, `data`,
`error`, and `progress`. This version describes the CLI result, not the persisted
task/setup schemas. `data` contains the command-specific result; on an exception it
is `null`. `command` can be `null` if argument parsing could not identify a command.
`error` is `null` on success, otherwise an object containing a stable coarse
`code`, readable `message`, `phase`, optional structured `details` (or `null`), and
`recovery` instructions. Do not parse message text or assume errors include stack
traces, subprocess output, or private dotenv contents.

| Outcome | Meaning | Exit / `ok` |
| --- | --- | --- |
| `completed` | Requested action finished; explicit `--no-setup` is a successful skip. | 0 / true |
| `queued` | Herdr accepted a worker; the action is **not confirmed finished**. | 0 / true |
| `cancelled` | The user declined confirmation; no lifecycle mutation was attempted. | 0 / true |
| `failed` | An error occurred before a tracked lifecycle mutation, or application health checks failed. | 1 / false |
| `partial` | Setup is incomplete/unmanaged, or a lifecycle mutation was attempted before an error. | 1 / false |

For `status` and `list`, `partial` also means individual record files could not be
validated. These read-only commands keep validated rows in `data` and add
`data.recordIssues` with relative `path`, `code: "INVALID_RECORD"` and a generic
`message`. The envelope error is `RECORDS_INCOMPLETE`. An omitted record may still
own resources: do not treat a partial inventory as permission to allocate, adopt,
or delete. Healthy reports are unchanged. Unsafe category directories and other
report-wide failures still return `failed` with null data. See
[incomplete reporting](diagnosis.md#incomplete-status-and-inventory).

Always inspect `outcome`: `ok: true` means the invocation was handled successfully,
not necessarily that the requested worktree is ready or removed. Queued results keep
the worker's `data.logId` where available; follow `graftr status` and Herdr plugin logs.
`create`, `setup`, and `open` retain setup status, stage, dependencies, path/workspace,
timings, and any dispatch log ID in JSON mode. Incomplete/unmanaged setup now returns
exit 1 from **all three entry points**, including default-output `create`; a retained
plain checkout is not successful managed setup. Explicit `create --no-setup` remains
exit 0. Health with no configured checks remains `not-configured`, not unhealthy.

**JSON inventory migration:** `graftr list PROJECT --json` now uses this envelope.
Its former inventory is unchanged inside `data`; update consumers from `.worktrees`
to `.data.worktrees` and from `.project` to `.data.project`. Scripts parsing the old
default JSON from public commands must add `--json` and read `.data` instead of the
unwrapped object. The explicit `--json` result is unchanged.
Help/bare invocation still produce text, and explicit help wins even with `--json`.
Completion generation and internal plugin events do not support JSON mode.

`freeze`, `frozen`, `thaw` and `schedule` also support the result envelope. Status
includes a `frozen` array alongside active `tasks` and `setups`; frozen entries hold
no port leases. A queued freeze is not finished. Thaw failures return `THAW_ATTENTION`
with retained recovery state. Freeze/schedule progress includes `save-frozen`,
`schedule-thaw` and `cancel-thaw`: timer activation/cancellation can be partial, just
like checkout operations. See [Freeze and scheduled return](freezing.md).

`graftr plugin install --json` reports the installed plugin ID, managed
directory and hooks. Progress includes `write-plugin` and `link-plugin`; a link error
can be partial and does not prove that Herdr registration failed. Inspect
`herdr plugin list` before retrying. Installation never runs startup hooks. The
plugin's startup worker reconciles and checks scheduled returns by default;
top-level `startupThaw: false` disables only that startup return check.

`graftr transfer TARGET --to HOST --json` adds optional `data.timings` with
`elapsedMs`, `stagesMs`, and receiver-reported `destination` timings. These are
recorded operation durations, not readiness or permission to replay effects;
cached acknowledgements retain their original timings. Timed transfer progress
also has `elapsedMs` on completed and uncertain steps. See
[transfer timing and recovery](transferring.md#retries-output-and-recovery).

Configured sender-side `hooks.afterTransfer` adds optional `data.afterTransfer`
with `status` (`complete`, `failed`, `incomplete`), safe hook observations and any
failure message, plus `data.destinationStatus: "ready"`. Sender follow-up failure
is partial with `TRANSFER_ATTENTION` even though the destination is ready. Warnings
include `data.retryCommand`. Ordinary cached retries never rerun these commands;
incomplete/running observations need explicit review. `transfer --retry-hooks`
deliberately resumes only unfinished hooks with the original context, skips
successes and reports `data.followUpOnly: true`; its timings cover only the retry.
`data.afterTransfer.plannedHooks` holds the original names/order and `attempt`
increments from 1. Prior results are preserved privately. Hook output stays in
sender-private logs. See
[transfer follow-ups](hooks.md#transfer-follow-ups).

### Partial failures and safe recovery

`progress` records ordered lifecycle stages attempted **in this invocation**:

```json
[
  { "action": "run-hook", "target": "my-project--fix-login", "status": "completed" },
  { "action": "save-task", "target": "my-project--fix-login", "status": "completed" },
  { "action": "close-workspace", "target": "my-project--fix-login", "status": "uncertain" }
]
```

`completed` means the stage returned, not that it necessarily changed something or
proves the current external state. `uncertain` means it threw; for example, a workspace
may close before its RPC response is lost. Successful steps followed by a dirty-Git
recheck failure also produce `partial`, even if there is no uncertain mutation step.
Reconcile results can include successful earlier targets before a later target fails.
An incomplete setup receipt from an earlier request can yield `partial` with no new
mutation steps. These distinctions prevent either a false success or a false claim
that nothing happened.

Progress is not a transaction/rollback log or an inventory of current resources. It
excludes lock/intent files, timing history, cosmetic label changes, and arbitrary
side effects of installers or approved health scripts. Read-only status/list and
application checks do not gain lifecycle mutation stages. Reporting errors does not
probe Git/Herdr or hook resources again or mutate resources to manufacture a recovery result.

Error phases are `arguments`, `state`, `target`, `preflight`, `confirmation`, and
`operation`. Error codes include `INVALID_ARGUMENTS`, `STATE_UNAVAILABLE`,
`CONFIG_UNAVAILABLE`, `INVALID_JSON`, `INVALID_CREATION_CONFIG`, `TARGET_NOT_FOUND`, `TARGET_AMBIGUOUS`,
`TARGET_RESOLUTION_FAILED`, `PREFLIGHT_FAILED`, `CONFIRMATION_REQUIRED`,
`CONFIRMATION_FAILED`, `SETUP_INCOMPLETE`, `SETUP_UNMANAGED`, `HEALTH_UNHEALTHY`,
and the fallback `OPERATION_FAILED`. Ambiguous selector details contain only public
candidate IDs/projects/branches/paths. Keep room for new codes; unknown codes are still
failures, not permission to bypass checks.

Review `error.recovery`, retained records/leases, Git, Herdr, and hook resources before
retrying a partial/uncertain action. Never automatically replay or undo uncertain
steps. JSON mode adds no `--force` or confirmation bypass: remove still needs a
terminal or its existing explicit `--yes`/native-action authorization, and nuke
still needs typed branch confirmation in a terminal. Prompts and optional diagnostic
warnings use stderr; stdout stays a single result document.
