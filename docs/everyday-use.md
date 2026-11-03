# Everyday use

Part of the [Graftr guide](../README.md).

## Standalone CLI and optional attachment

From a plain terminal, configured `create`, `setup` and `open` work without a Herdr
executable, socket or server. They run the shared preparation pipeline synchronously;
there is no new daemon or application supervisor. Git and Linux `flock` remain
required; Tailscale is needed only by hooks that use it.
A standalone create requires a valid configured repository and
worktree root; it cannot guess a native workspace source when configuration is missing.

Automatic selection is the default; no flag is normally needed. Configure the optional
**top-level** `herdr` setting in machine-local `projects.json`, alongside `schemaVersion`
and `projects` (not inside an individual project profile):

```json
{
  "schemaVersion": 2,
  "herdr": "auto",
  "projects": {}
}
```

Keep your actual project profiles in `projects`; the empty map above shows placement only.
- `auto` (also the default when omitted): without a complete Herdr context, prepare
  locally. With a contextual socket, make a read-only workspace-inventory request with
  a 1.5-second deadline before any mutation. Missing/refused/disconnected/timed-out
  transport selects standalone preparation. This does not depend on finding a Herdr binary.
- `off`: standalone create/setup/open without probing Herdr; native create/open hooks
  skip automatic enrollment/attachment. Explicit plugin preparation actions refuse
  rather than preparing their worker directory. Cleanup of existing attachments remains enabled.
- `required`: refuse create/setup/open when Herdr context or its connection is unavailable.

Permission, ownership, configuration, malformed-response and explicit server-refusal
errors are not treated as absence. Malformed/unreadable configuration cannot safely
choose a policy and fails before creation; a missing file retains the old native
unconfigured-project fallback when running through an available Herdr connection.
Once an adapter is selected, later failures do not switch providers. In particular,
lost creation/open replies may mean a workspace was already created: inspect before
retrying. Explicit Herdr plugin actions always require their selected workspace and
never automatically fall back to cwd. Automatic fallback retains any known attachment;
it is not a detach or a deletion bypass. Readiness/output/exit semantics are unchanged.

```bash
graftr create my-project my-task
cd /configured/worktree/root/my-task
graftr setup                          # reuse installation and existing private .env
graftr open my-project--my-task              # prepare/reopen locally, not a new shell
graftr create my-project another --no-herdr  # also ignore inherited Herdr context
```

To attach later, run `graftr open my-project--my-task --wait` from a real Herdr pane with
the parent repository workspace available. Herdr opens the same verified checkout;
enrollment reuses its task ID, environment, receipt lineage, installation and port
leases. Native Herdr creation still creates through Herdr and enrolls that exact path.
Native opening of an already managed standalone checkout attaches it through the
same pipeline; opening an unmanaged checkout does not silently enroll it.
Attachment waits for an active standalone installer outside the registry lock. A
stale pending attempt is refused rather than silently replayed; explicitly retry setup.

`--no-herdr` is available only on create/setup/open as a one-command override of any
configured mode. Explicit plugin actions refuse it. It skips workspace RPCs, tabs and
notifications, but does not erase a previously recorded attachment. Read-only local
operations need no live server. Destructive operations for a **known attachment**
retain their existing server/ownership checks: an unavailable server is not evidence
that its workspace disappeared. `label` requires an attachment; `reconcile` skips
unattached tasks. Standalone close keeps files/leases and runs recorded cleanup hooks;
standalone remove needs no plugin and can run from inside the checkout. Your terminal
is not closed and may remain in the deleted directory afterward; `cd` elsewhere.

Herdr retains its native close/remove ordering, confirmation and force decisions.
Graftr's post-event cleanup now records closure and fences setup before auxiliary
cleanup; native removal waits for the setup lock before retiring leases. See
[Native close/remove cleanup](#native-closeremove-cleanup) below for failure/retry and active-installer limits.
Unavailable known attachments are still not silently detached or force-deleted.

## Getting help

Run `graftr` with no arguments for a grouped command overview and two basic examples.
Command-specific arguments and options are kept out of that overview:

```bash
graftr help getting-started # prerequisites, a minimal profile, and your first task
graftr help recovery        # reversible actions and permanent deletion limits
graftr create --help        # arguments, creation options, and examples
graftr help create          # equivalent command help
graftr remove -h            # deletion behavior and safeguards
```

`graftr --help`, `graftr -h`, and `graftr help` show the same overview. Every public
command has its own help page. Explicit help takes precedence over invalid options
and never runs the requested operation, reads configuration, or connects to Herdr.
Invalid invocations still fail, with a hint pointing to the relevant help page.

The layout follows Git's task-oriented command groups, GitHub CLI's
[command-specific help](https://cli.github.com/manual/gh_help), and the
[Command Line Interface Guidelines](https://clig.dev/#help): keep the entry point
concise and put examples, options, and operational details at the next level.

## Choosing a worktree and shell completion

`open`, `close`, `cleanup`, `health`, `label`, `remove`, and `nuke` accept an exact
managed checkout folder name, recorded Git branch, checkout path, or existing full ID:

```bash
graftr open fix-login
graftr open task/fix-login
graftr open ../worktrees/my-project/fix-login
graftr open my-project--fix-login
graftr cleanup fix-login --project my-project
graftr help targeting
```

Short names/branches are scoped to the current managed project or Git repository.
Outside a known project they must be unique across the registry. `--project` selects
an explicit project; ambiguous folder/branch/path matches fail with candidate details,
never fuzzy/prefix matching or an arbitrary first result. An existing full ID takes
precedence. Explicit paths and full IDs can address another project unless `--project`
restricts them. Existing identifiers, paths, branches, port leases, and state schemas
are unchanged; this does not adopt unmanaged worktrees or rebind a changed branch.

Omit the target (or use `@`) to select the canonical current managed directory,
including nested directories, then the caller's owned Herdr workspace. Native plugin
actions keep their selected workspace rather than their worker's cwd. `nuke` requires
an explicit target, refuses `@`, and retains all typed-confirmation/clean/merged checks.

With the standard `bash-completion` framework installed, generate the completion
script into your user completion directory:

```bash
mkdir -p ~/.local/share/bash-completion/completions
graftr completion bash > ~/.local/share/bash-completion/completions/graftr
```

The generator prints the exact same script without installing it or editing shell
configuration. Completion supports public commands, scoped options, configured project
names, managed names/branches/IDs/paths, and local refs for `create --base/--branch`.
Checked-out branches are omitted from `--branch` suggestions. Completion reads metadata
and local Git refs only, never Herdr/Tailscale, installers, health checks, or servers.
Invalid/missing state gives no dynamic suggestions or error noise. Other shells are
not yet supported. Ownership and lifecycle checks still run when a command is executed.

## Create and prepare

Create from any terminal. For optional automatic attachment, use a Herdr pane connected
to the session containing the parent repository workspace. Alternatively use Herdr's
native **Create Worktree** for an opted-in project:

```bash
graftr create my-project fix-login
graftr create my-project another-task --base main
graftr create my-project scripted-task --wait # wait for setup before starting task commands
graftr create my-project plain-task --no-setup # plain checkout; bypass automatic setup
graftr status
graftr health my-project--fix-login # explicitly run this project's configured checks
graftr list my-project
graftr list my-project --json                # structured inventory for agents/scripts
```

CLI task names use lowercase letters, numbers, and single hyphens (up to 48 characters).
The default CLI branch is `task/<name>` and folder is `<worktreeRoot>/<name>`;
project `branchName`/`worktreePath` templates can change future CLI creations (see
[Creation naming templates](configuration.md#creation-naming-templates) or `graftr help naming`). Native creation keeps Herdr's chosen path and branch;
its managed task ID uses the folder name plus a short path hash to avoid name collisions.
The default CLI base is the configured repository's committed HEAD,
not its uncommitted working files. Standalone creation prints its preparation result
after completion, with no workspace ID. Inside Herdr, creation prints the path,
workspace ID, optional configured URL and a `pending` setup acknowledgement with its Herdr log ID.
The checkout/workspace is visible immediately; installation completes in an independent
Herdr one-shot worker. The workspace is grouped beneath the parent without moving focus. The `agent`,
`editor`, `terminal`, and `server` tabs are ordinary shells.

For a new configured Herdr checkout, parent-workspace discovery runs outside the
global registry lock. Before registering a parent or creating the checkout, Graftr
rechecks the project/policy, target-related task/adoption/frozen/setup records,
canonical destination, relevant Git registrations and branch tip under that lock.
A changed target requires review and a fresh request, not an automatic retry or
opening a task that another creator just claimed. Unrelated worktrees may proceed
during discovery. Parent registration, its post-registration verification and actual
checkout creation remain serialized; unconfigured native fallback and existing-task
reopening retain their previous locking behavior. No discovery plan is persisted.

By default, managed workspace labels put the secondary port first
when assigned, otherwise the local port: for example `5200 fix login` or
`3200 fix login`, rather than repeating the parent project's name. The port
stays visible when a narrow/mobile sidebar truncates a long task name. Native creation
and setup derive the task name from the attached branch, dropping its leading namespace
(such as `task/` or `feature/`) and replacing dashes and remaining slashes with spaces.
For example, `feature/fix-login` becomes `5200 fix login`, regardless
of the folder or previous workspace label. Legacy and manually edited labels are
replaced on the next setup/open; repeated setup does not duplicate the port. Projects
can change this format with `workspaceLabel`; see [Workspace label templates](configuration.md#workspace-label-templates).
The label is an assigned address, not proof the server or setup is ready. Plain/fallback
worktrees have no invented port. Existing open labels update on their next setup/open;
no folders, branches, internal `PROJECT--TASK` IDs, or existing port assignments are migrated.

## Reusing a checkout on another branch

Switching Git branches inside a managed checkout does not rename its folder or
rebind Graftr's historical ownership record. Ordinary `remove` can nevertheless
retire that clean checkout using its **verified current branch**, preserving all
existing branches. `nuke` retains its strict recorded-branch and merged-commit checks.

To update only the sidebar label after switching branches:

```bash
graftr label
graftr label fix-login --json
```

The Herdr workspace action **Refresh workspace label (current branch)** runs the
same command for its selected workspace, regardless of the worker's directory.
It uses the current attached branch with the existing `workspaceLabel` template;
the default keeps the secondary-port prefix, or local port when no secondary is assigned. Custom templates still control their own
format. Dirty work is permitted; detached, missing/unregistered, repository-mismatched
or ambiguously owned checkouts/workspaces are refused. A closed workspace is not
opened. Active/pending setup is excluded by the existing lock and receipt checks.

This changes only the live owned workspace's display label. It does not read or
write `.env`, run setup/installers/hooks/health checks, create tabs, publish URLs,
rename Git branches/folders, introduce aliases, or update ownership/setup records.
Lock files may be created. Other commands, including setup/open/health and
environment refresh, retain their recorded-branch rules; this is not automatic
retasking or environment migration. Use the folder/path/full ID or recorded branch
to target the checkout—the new label/current branch is not a new selector alias.

Refresh is one-shot, not a branch watcher. It rechecks branch/record/setup/profile/workspace
identity and verifies the returned live label. A failed/lost reply or late branch
change can leave an already-updated label; JSON reports failed/partial with
`refresh-label` completed/uncertain progress, without rollback or automatic retry.
Inspect Herdr and deliberately refresh again. An already-correct label is a no-op.

Herdr inventory reads run without holding the global registry lock, while the
target's worktree lock remains held. After those reads, Graftr rechecks the task,
setup receipt, profile, branch and unique ownership before trusting the result—even
for an unchanged label or a successful rename's final verification. The rename
itself stays serialized under the registry lock; Herdr has no conditional ownership
guard on that mutation. This avoids blocking unrelated operations during inventory
reads without treating a stale snapshot as permission to rename another workspace.

Both creation providers share enrollment/environment/install/URL setup; tabs are
attachment-only. Inside Herdr, CLI `create`, `setup`, and `open` queue setup by default,
and native hooks run asynchronously: a created
folder or accepted command does **not** mean setup has finished. Inspect
`graftr list my-project`, `graftr status`, or `herdr plugin log list --plugin local.graftr`.
Routine setup is quiet: pending/ready, plain-worktree fallback, explicit skips, and
intentional close/cleanup cancellation do not send Graftr toasts. A worker that records
a new managed setup failure sends a short attention notice only while that failure still
belongs to a live, active task. Duplicate workers reusing the failure do not notify again.
Details and retry guidance remain in CLI output, status/list, and plugin logs; plain
fallbacks are reported there rather than by popup. Failure notices are best-effort and
respect Herdr's toast settings. Agent completion/approval notifications are unchanged.
Use `--wait` inside Herdr with CLI create/setup/open when a script or agent must wait
for preparation to finish. Standalone commands already wait. Direct setup is also a
recovery option when plugin dispatch fails.
Opening an existing **managed** worktree through Herdr reruns its setup, including publish hooks, and binds
the new workspace identity. Opening an **unmanaged** worktree does not silently enroll it.

Once setup is ready, run your project's normal server command in the **server** tab,
for example:

```bash
pnpm dev
```

A published URL already points at this worktree's assigned port; it can return a proxy
error until your server listens there. Setup readiness is not application health. Graftr
never infers listener ownership, authentication or realtime connectivity; explicitly
configured `graftr health` checks can report selected application indicators separately.

```bash
graftr cleanup my-project--fix-login # run recorded cleanup hooks; keep workspace/files/servers
graftr close my-project--fix-login   # cleanup and close Herdr workspace
graftr open my-project--fix-login    # reopen/prepare at the same path and URL
graftr remove my-project--fix-login  # confirm deletion of a clean folder; keep branch
graftr reconcile                     # cleanup for missing owned workspaces
```

Inside an owned task workspace, `open`, `cleanup`, and `close` can omit the task ID.
`remove` can also resolve the current managed folder, including from a plain terminal:

```bash
graftr close                          # close this task's workspace; keep its files
graftr remove                         # confirm full retirement; keep its branch
```

Closing your own workspace hands off to Herdr's independent, one-shot plugin runner.
That worker closes the workspace, saves the closed record and finishes recorded cleanup hooks
even after the caller's pane disappears. The caller resolves its explicit pane rather
than guessing from cwd or another client's focus; inherited workspace IDs can be stale
after pane moves. Uncommitted work, the checkout, branch, `.env`, and port leases remain.

The `local.graftr` plugin must be linked/enabled, with `graftr` on Herdr's PATH. The
caller and plugin runner must use the same machine-local XDG configuration/state
locations. A failed dispatch never falls back to closing its own caller directly.
The optional JSON receipt (`status: "closing"`, with `logId`) means **accepted**, not
completed; the pane may close before it prints. From another workspace, inspect
`graftr status` and `herdr plugin log list --plugin local.graftr` if cleanup fails.
Explicit close from another workspace and native plugin close actions complete directly.

Direct close reads Herdr inventory without holding the global registry lock, then
rechecks the task, setup receipt and unique ownership before cancelling setup,
queuing cleanup or closing the workspace. A concurrent change requires review and
a fresh close request; it does not cancel a newer setup from a stale snapshot.
The actual close request stays serialized. Pending setup is fenced before any wait
for the installer's worktree lock; this does not claim to terminate that installer.

Herdr workspace actions expose **Set up worktree / retry setup**, **Set up worktree
(skip dependency install)**, **Open / prepare worktree (full setup)**, **Run worktree
cleanup hooks, e.g. unpublish URL (keep files)**, **Close worktree workspace (keep
files)**, **Refresh workspace label (current branch)**, and **Delete clean worktree
folder (keep branch)**. Normal workspace close triggers cleanup asynchronously.
Herdr owns ordinary pane shutdown; detached/external servers remain your responsibility.
Client detach and individual pane exit do not trigger graftr cleanup.

## Native close/remove cleanup

Herdr emits its native close/remove events after performing the operation; Graftr's
asynchronous hooks do not impose the CLI's clean-worktree checks on native force
removal and never replay a destructive native request. The explicit Graftr **Delete
clean worktree folder** action still uses Graftr's own non-force/dirty-work protections.

For a verified owned event, Graftr persists `closed` and cancels a pending setup
request before running cleanup hooks. A cleanup failure therefore cannot
leave a natively closed workspace recorded as ready or allow that request to publish
readiness afterward. Close retains the checkout, environment and leases.
Reconciliation and workers that detect a missed close event use the same closure-first
ordering. An unavailable workspace inventory still does not prove closure.

Native removal first verifies that the matching path and Git registration are gone.
After fencing setup, its event worker waits for
the per-checkout setup lock **outside** the shared registry lock. Other worktrees and
the setup worker can continue. Only after obtaining that lock does it recheck the
current record identity, closed state, canonical path and Git disappearance. It runs recorded cleanup hooks outside the registry lock before
clearing the receipt, deleting the record and releasing leases. Native close also
waits for active setup when cleanup hooks are pending; files and leases remain.
Duplicate/out-of-order hooks do not resurrect deleted records or retire a newer
attachment. Conflicting records/snapshots fail before resource mutation.

This is publication fencing and delayed retirement, **not installer termination**.
An active installer/hook may finish writing. If it recreates the removed path, a
new registration/attachment appears, lock acquisition times out (currently 660
seconds), or cleanup fails, records/leases remain for review. Arbitrary application
processes are never discovered or signalled. Native removal cannot undo files it
already deleted; Graftr neither recursively removes late output nor retries force.

Valid cleanup hooks print a separate internal report in Herdr's plugin log:
`nativeStatus: "completed"` plus `cleanupStatus: "complete"`, `"incomplete"`, or
`"ignored"`. `ignored` means no currently matching ownership was acted on. A valid
post-event cleanup failure exits zero and reports `incomplete`, rather than presenting
the completed native action as failed. Invalid/unverifiable envelopes and ownership
conflicts still fail validation. Service/record contents are not echoed in the report.

After a cleanup failure, inspect `graftr status` / `doctor` and the plugin log. Resolve
the service/ownership issue before explicitly retrying `cleanup` for a retained
checkout or `remove` for a missing checkout after setup finishes. Do not delete
records to free ports, replay native deletion, or discard a late recreated directory
without reviewing its contents. A known unreachable attachment still blocks destructive
CLI retirement; automatic standalone preparation does not erase that attachment.

## Undo and recovery

| Action | Recovery |
| --- | --- |
| `cleanup` | `setup` (or `open` for a closed workspace) reruns setup hooks and republishes at the same URL; a reused install receipt skips installation. |
| `close` | `open` reopens/prepares the same checkout with its retained port assignments. |
| `remove` | `create PROJECT TASK --branch BRANCH` checks out the retained local branch in a new worktree. |
| `nuke` | No automatic undo; the checkout and local branch are deliberately deleted. Merged commits remain reachable through the reviewed main-checkout branch. |

Cleanup cancels pending setup; a later setup does not pretend a cancelled installation
completed. Neither command starts or stops servers.

To resume a removed task's committed work:

```bash
graftr create my-project fix-login --branch task/fix-login --wait
```

`TASK` still controls the new CLI folder and internal task ID; `--branch` names the
actual branch to check out and the label remains branch-derived. This accepts exact
local branch names only, never a tag, revision, remote fetch, or forced checkout.
The branch must exist and must not be checked out or claimed by another managed task.
Use either `--branch` or `--base`, not both. Normal create still refuses automatic
branch adoption. Existing task IDs are reopened only if their branch matches the
explicit request. A configured repository/worktree root is required for branch reuse;
it does not fall back to an unrelated caller repository.

This reconstructs a checkout and development environment, not deleted ignored files.
Ports are allocated again and may differ. Installer side effects are not generally
reversible; Graftr does not roll back arbitrary trusted project commands.

Repeating `create` or `open` preserves the branch, `.env`, port assignments, and surviving
tabs. Interrupted preparation can resume if its owned checkout exists. If Git creation
failed before that point, inspect the record and Git worktrees rather than deleting
anything automatically. `ready` means resources are prepared, **not** a server is live.

### Parent repository association

A terminal can already be in the configured repository while its Herdr workspace lacks
the recorded worktree-group association. Creating another ordinary workspace does not
add that association and can leave a redundant workspace pointing at the same checkout.

For a valid project profile, `graftr create` first uses its unique associated parent.
When that association is missing, run creation from the existing primary-repository
workspace. Graftr verifies that explicit caller through Herdr's read-only worktree
discovery, reopens the same checkout without changing focus to register the association,
and verifies that Herdr reused the same workspace before creating the task. Normal
configured environment, dependency installation, and URL preparation still follow.
An ambiguous parent, a caller in another repository or a linked child, and discovery or
registration failures stop creation with an error; they do not silently produce an
unmanaged checkout. Graftr neither trusts workspace labels nor uses another client's focus.

## Fallback, retry, and installation

Creation is separate from development setup. Missing/invalid profile values, an absent
baseline, unavailable tooling/Tailscale, occupied ports, or a failed installer leave the
checkout and any Herdr workspace intact. Graftr never rolls back by deleting work or guesses
credentials/origin-variable names. It reports `unmanaged` before enrollment or `incomplete`
after enrollment, with the failed stage and retry guidance. Existing `.env` files are
never overwritten to make a retry pass.

With Herdr selected, when CLI repository/root settings are unavailable, `create` uses the **calling Herdr
workspace** as the native source and lets Herdr choose its default checkout path. The
project label does not identify a repository in that fallback: check the printed path.
No other client's focused workspace is used. Invalid Git refs, conflicting/existing
paths, unavailable caller/server identity, and unsafe metadata permissions remain errors;
fallback is not permission to weaken Git or ownership protection. Standalone creation
requires valid configuration instead of using this native-only fallback.

Inside the worktree, or through its Herdr setup action:

```bash
graftr setup                          # enroll or retry after fixing configuration
graftr setup --skip-install           # prepare env/URL without the installer
graftr setup --install                # explicitly run/rerun the configured installer
graftr setup --wait                   # finish synchronously without a plugin handoff
```

`create --no-setup` preserves an explicit skip receipt, so its own creation hook cannot
enroll it by accident. Later explicit `setup` opts it in. `autoSetup: false` disables
native automatic enrollment for an entire project; CLI create and manual setup remain
explicit opt-ins. Completely unconfigured repositories stay outside automatic state;
their fallback appears in the hook log. Existing unmanaged worktrees stay untouched
until explicit setup, even if automatic setup is subsequently enabled.

Skipping setup does not bypass registry locking or branch/path reservations. Plain
creation waits for the same registry lock as managed creation and checks the current
reservations after acquiring it. Its creation intent remains active until the skip
receipt is saved, so an early Herdr creation event can return without waiting on
that lock or accidentally running setup.

An optional `installCommand` is an approved **argv array**, not inferred from a checkout
script or passed through a shell. It runs once after environment verification, with public
worktree overrides, closed stdin, and a ten-minute limit. Installer stdout/stderr go only to a
private run log, never to CLI output or Herdr logs, so credentials it prints are not spread
around. On failure the message names that log; read it with `graftr logs --latest` (see
[Run logs](diagnosis.md#run-logs)), fix the cause, then retry. A successful receipt
fingerprints the command, public overrides, platform/architecture, resolved installer/Node/Bun
executable identities, selected install-mode environment (`NODE_ENV`, `CI`, and npm production/
omit/include flags), and dependency inputs. Inputs include root and Git-visible workspace
`package.json` files, pnpm/npm/Bun/Yarn lockfiles, package-manager configuration, pnpm hooks,
and `patches/*.patch` files. Root standard input paths are checked even when Git-ignored;
ignored workspace inputs require explicit `installInputs`. File contents are hashed, never
logged or stored in receipts. Dotenv files are not fingerprinted, and input symlinks are
refused rather than followed outside the checkout.

The receipt also records existing root/workspace `node_modules` directories and common
package-manager markers. Reuse requires matching inputs and output presence/types. Changed
inputs or missing recorded outputs rerun the installer, as do changed public assignments or
explicit `--install`, unless installation is explicitly skipped. This is a lightweight
layout check, not an integrity audit of every installed package file. Changing only template
spelling without changing resolved assignments does not reinstall. Legacy command-only
receipts remain readable but refresh once on their next non-skipped setup.

For custom installers, optional `installInputs` adds literal relative file paths to hash;
optional `installOutputs` replaces automatic output discovery with required literal relative
file/directory paths (for example `["node_modules", "generated/client"]`). Empty explicit
outputs mean fingerprint-only reuse. Paths cannot escape the checkout, contain globs or
name dotenv files. Custom installers that produce no discoverable Node layout should name
their required outputs explicitly. Inputs are checked before and after installation; if
they change, setup stays incomplete with no reusable success receipt. This includes an
installer generating/changing a lockfile: review the result and retry setup to verify it.
No install command means a clearly reported manual-install fallback, not an inferred command.

`--skip-install` persists for that checkout until `--install` clears it. Setup rechecks
workspace ownership after installation: closing through Herdr cannot cause a background
hook to reopen shells or publish a URL afterward. Close and cleanup invalidate the setup
request, including a queued worker that has not started. A later explicit setup/reopen
gets a new request identity; an old worker cannot mark it ready. A cancelled install may
finish writing dependencies, but its result is not claimed as successful setup; a later
setup may run installation again. Graftr does not stop that installer or supervise servers.

Installation holds only a per-checkout kernel lock, not the shared registry lock.
Other worktrees can be created, installed, cleaned up, and closed independently. Duplicate
workers wait only for the same checkout and reuse a completed setup. Deletion through
Graftr refuses immediately while that checkout's setup lock is held, without changing
its workspace/files. Wait for completion and retry. Herdr's own native force
decision is independent; its post-event cleanup fences publication and delays lease
retirement until setup releases its lock, but cannot promise to stop installer writes.
Stable lock files are retained after
removal so an existing waiter cannot acquire an obsolete lock inode.

`create`, `setup` and `open` return nonzero when preparation remains incomplete or
unmanaged, even if a useful checkout remains. Inspect `data.status`, `step` and
`dependencies` in the JSON envelope. Explicit plain-checkout skips and ready results
return zero. Accepted background setup also returns zero: pending means accepted,
not complete. Standalone commands finish preparation before returning. Asynchronous
workers log the outcome without making an already-completed native creation fail.
`status --json` includes private setup receipts; plain `status` summarizes them,
and `list` displays the last setup result
separately from current lifecycle state.

## Optional application health

Health checks are disabled unless a project's approved profile supplies `healthChecks`:

```json
"healthChecks": [
  { "name": "app", "type": "http", "path": "/health", "expectedStatus": 200 },
  { "name": "listener", "type": "tcp", "timeoutMs": 1000 }
]
```

Run `graftr health TARGET`, or omit the target inside its managed directory or owned
Herdr workspace. With a directory or explicit target, a live Herdr connection is not required. Ordinary `status`, setup,
open and cleanup never execute these checks. The JSON report keeps `setupStatus`,
`taskStatus` and `health.status` separate; no configured indicators means `not-configured`,
not unhealthy. Configured checks all need to pass for `healthy`. Exit status is zero for
healthy/not-configured and one for unhealthy or an invalid request. Results are point-in-time
observations, not persisted health guarantees, and do not rewrite lifecycle/setup records.

HTTP checks issue GET requests to `127.0.0.1` on the task's assigned local port, expect
200 by default, and never follow redirects or log response bodies. TCP checks only test
whether that same port accepts a connection; they cannot identify the application or prove
process ownership. Arbitrary remote destinations/auth headers are not supported.

A command check uses an explicitly approved argv array, for example
`{ "name": "project", "type": "command", "command": ["pnpm", "run", "health"] }`.
It runs in the verified checkout with inherited environment plus the task's public overrides,
closed stdin, and discarded stdout/stderr; Graftr does not load `.env` into it. It is trusted
project code running as your user, **not a sandbox or an automatically discovered script**.
Use finite, non-daemonizing checks, not server-launch commands. On timeout, Graftr kills
only the process group created for that one-shot check, not arbitrary listeners or servers;
commands that deliberately detach descendants are not contained by this mechanism.

Checks run sequentially, with a default two-second timeout per check, configurable from
100 to 30000 milliseconds. Profiles allow at most 16 uniquely named checks. Setup/removal
and health commands share the per-checkout lock, so health refuses an already-running
setup rather than probing a changing checkout. No daemon, recurring probe, PID adoption,
server lifecycle management or actual project health configuration is added automatically.
