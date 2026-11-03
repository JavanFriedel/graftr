# Freeze work and return later

Part of the [Graftr guide](../README.md).

`close` keeps everything on disk. **Freeze** keeps the local branch and a small
private recovery record, while removing the checkout and releasing its ports.
**Thaw** recreates that checkout and runs ordinary setup from current configuration.

```sh
graftr freeze my-project--fix-login
graftr frozen
graftr thaw my-project--fix-login
```

Freeze has the same ownership and clean-Git safeguards as `remove`. Commit or move
tracked/untracked work first. Confirmation describes the folder, current branch and
commit. `--yes` skips confirmation, not safeguards. A workspace freeze action also
counts as explicit authorization. From inside its Herdr workspace, freeze hands off
to an independent plugin worker; a `freezing` response means accepted, not finished.
Inspect `graftr frozen` and `graftr status` from another workspace.
This first version freezes only checkouts beneath their configured `worktreeRoot`,
so the normal creation safeguards can recreate them. Other explicitly enrolled
checkouts can still use `close`; review their project layout before freezing.

## What survives

- The observed local Git branch, plus a record of its exact commit and original path.
- Task identity, the explicit dependency-install skip, optional return date and note.
- No port leases while fully frozen. Frozen recovery records do not expire in `prune`.

**This is not a backup.** The branch remains local; deleting it externally can prevent
thawing. Graftr refuses a missing or moved branch rather than resetting it. Thaw keeps
the original checkout path even if naming templates change, but the current configured
repository/worktree root must still own it. Branch/path/task reservations prevent
ordinary `create` or enrollment from silently claiming frozen work.

**Ignored files are deleted**, including `.env`, `node_modules`, caches, ignored notes
and local data. Save anything valuable first. Environment values are not archived:
thaw uses the current approved baseline/configuration and new available ports/URLs.
Local `.env` edits are lost. Dependencies can resolve differently on reinstall.

Freeze runs both cleanup and **retire hooks**, just like removal. Configured task
databases or other external resources may be deleted. Thaw provisions fresh resources;
it does not restore their data. Detached/external servers are not stopped.

## Optional scheduled return

Scheduling is an explicit Linux/systemd add-on, enabled per task with `--until`:

```sh
graftr freeze fix-login --until 2026-11-06T09:00:00-05:00 \
  --note "Retry after the upstream release"
graftr schedule fix-login --until 2026-11-09T10:00:00-05:00
graftr schedule fix-login --cancel
```

Use a future ISO timestamp with an explicit timezone, not natural-language dates.
Notes are single-line, at most 2000 characters, and must not contain secrets.
The task must already have a Herdr attachment; open/attach it before scheduling.
An unscheduled freeze does not contact systemd. Manual `thaw --no-herdr` is available
for synchronous local preparation without a workspace.

Each scheduled task gets its own `.timer` and `.service` under
`$XDG_CONFIG_HOME/systemd/user` (normally `~/.config/systemd/user`). `OnCalendar` names
one absolute UTC date, with `Persistent=true`: no every-minute polling, transient
timers or always-running Graftr daemon. Units survive reboot and catch missed returns
when the user service manager next starts. Graftr does not enable lingering, root
services or waking the machine; a return can be late.

The service records absolute Graftr executable/source, configuration and state paths,
and the scheduling shell's `PATH`. It does not save other shell environment variables
or credentials. Use the standalone executable for schedules that should survive source
directory changes. Hooks/installers still need their executables and appropriate
credentials available in the service's environment. No `.env` is sourced into hooks.
Cancel schedules before moving the configuration/state directories (including a
legacy-directory migration), then reschedule afterward so saved paths are current.

When due, Graftr recreates/prepares the checkout synchronously and opens its recorded
Herdr workspace **without stealing focus**. A `welcome back` tab is moved to the
first position and explains the return, note and setup outcome. Existing tab and
workspace selection is preserved. Press Enter or close the tab to dismiss it. Graftr
does not start agents or development servers, and sends no separate notification.

If the recorded Herdr server is unavailable or its parent repository workspace is
not open, no checkout/setup is attempted. Graftr schedules another one-off timer with
increasing delays: 5, 10, 20 minutes, up to 6 hours. Only pending scheduled work retries.
Invalid replies, permission/ownership errors and failures after setup starts do not
trigger automatic setup replay. An interrupted `thawing` record also requires review.

Manual thaw, cancellation and rescheduling fence stale timer requests. Thaw and the
startup check share a per-task lock, so they cannot create two workspaces concurrently.
Successful/manual thaws remove their timer/service files. Cancellation leaves work
frozen. If scheduling fails, the frozen record survives; retry `graftr schedule` or
thaw manually. Inspect timers with `systemctl --user list-timers` and service failures
with `journalctl --user -u UNIT.service` (the unit name appears in the timer list).

## Herdr startup check

The existing Graftr plugin reconciles owned workspaces and checks overdue scheduled
returns by default at server startup/handoff. Install it through Graftr:

```sh
graftr plugin install
```

This command copies the bundled manifest into private user data and links/enables it
through Herdr's CLI. It works without the source checkout; no server restart or
immediate overdue-thaw check is performed during installation. For editable plugin
development, `herdr plugin link ~/graftr/plugins/herdr` remains available.

Its single startup command is `graftr _startup`, which reconciles first and then runs
the overdue-return check for that exact socket/server. It ignores unscheduled/future
tasks, other servers and failed/interrupted setup. A reconciliation failure is reported
without preventing independent scheduled returns from being checked. The startup
check can return work sooner than the next deferred timer.

Set top-level `"startupThaw": false` in `projects.json` to disable startup thawing.
The field defaults to `true` and must be a boolean when present. This is runtime
configuration, not an installer flag or a separate plugin. Reconciliation still runs,
and explicit `graftr thaw --due` and per-task systemd timers are unchanged. The global
`"herdr": "off"` setting also suppresses startup thaws. Cancel a task's automatic
return explicitly with `graftr schedule TARGET --cancel`.

## Recovery

Private version-1 records live in `state/frozen`, separate from active schema-4 tasks
and leases. Freeze writes its intent before removal. A failed cleanup/retire hook keeps
the active ownership/leases; review effects, then explicitly retry `graftr freeze`.
A crash after removal may leave only a `freezing` recovery record: the same command
can verify retirement and finish it without replaying deletion.

A failed thaw keeps its checkout, task/setup receipt and frozen recovery state.
Inspect `graftr frozen`, `graftr status`, and `graftr logs TARGET`; resolve the cause
before explicitly retrying `graftr thaw TARGET`. No branch reset, automatic rollback,
forced deletion or automatic hook replay is provided. Completed recovery records stay
private so the welcome pane can read them; they are hidden from the frozen listing
and replaced if that task is frozen again.
