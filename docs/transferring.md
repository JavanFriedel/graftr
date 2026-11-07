# Transfer work between machines

Part of the [Graftr guide](../README.md).

Transfer an unfinished feature over an existing SSH connection, including one
provided by Tailscale:

```sh
graftr transfer my-project--fix-login --to laptop
# Later, on laptop:
graftr transfer my-project--fix-login --to desktop
```

These are copy-first handoffs, not continuous synchronization. The source's files,
branch, index, workspace, ports and services remain in place. Do not keep editing
both copies: returning work requires the earlier source's portable snapshot to be
unchanged. Graftr never silently merges independent edits, forces a checkout reset,
pushes commits, or removes the source.

## Prepare each machine once

- Both machines must run Linux and compatible Graftr versions with this command.
- Normally configure the **same project name** on both machines, pointing at matching
  repository clones. Paths, worktree naming, environment baselines, ports, installers
  and hooks may differ. The destination must already contain a root commit shared
  with the feature's history. A missing destination project can instead be created
  through the explicitly approved [project migration](#migrate-a-missing-project) below.
- Make `graftr` available on the remote noninteractive SSH command's `PATH`.
- Establish SSH authentication and trusted host keys first. For example,
  `ssh laptop graftr --version` should succeed without prompting. Graftr uses
  batch authentication and strict host-key checking; it does not enroll host keys,
  configure Tailscale, forward an SSH agent, or bypass your SSH trust settings.

`--to` accepts an SSH config alias, hostname, Tailscale hostname, IP address, or
`user@host`. Use your SSH config for ports, keys and jump hosts. No daemon, cloud
account, intermediate Git remote, or Tailscale API integration is needed.

The destination is restored before its normal trusted project installer/hooks run.
It uses its own baseline, available ports and optional Herdr server. Applications
are not started. Use `--no-herdr` to prepare without workspace attachment. A
workspace-attachment failure does not discard successfully received work.
An ordinary SSH command has no Herdr pane context: `herdr: auto` then prepares
standalone. Open it from a destination Herdr pane afterward if desired. A supplied
destination-local Herdr context uses the normal owned-workspace setup; the source's
socket/workspace identity is never forwarded. `herdr: required` without that context
needs attention unless `--no-herdr` was explicitly selected.

## Dry run before sending

```sh
graftr transfer my-project--fix-login --to laptop --dry-run
graftr transfer . --to laptop --include notes.local --no-herdr --dry-run --json
```

A dry run checks the source Git state, portable file paths/types, SSH/protocol,
destination project/history, branch/path reservations and collisions. For a return,
it also compares the retained source's HEAD, staged entries, literal working bytes,
executable flags and symlinks with its outgoing snapshot. Timestamp-preserving edits
do not bypass that comparison. Destination baseline-file presence and directly
resolvable installer executables are checked without reading environment values or
executing project commands.

Neither machine's project, checkout or Graftr state is changed: no persistent locks,
snapshots, bundles, receipts, port leases, configuration writes, installers, hooks,
health checks or workspace attachment. Small protocol requests use anonymous memory,
not temporary files. SSH itself still uses your existing authentication/trust setup.

Passing preflight is **not a guarantee of successful setup**. Findings explicitly
leave installer/hook execution, actual port allocation, exact compressed bundle size,
final environment verification and concurrent writers unchecked. Actual transfer
independently rechecks its safeguards. The dry-run file inventory is bounded to
16 MiB; this is separate from the normal transfer's payload limit.

JSON reports `data.dryRun: true`, `data.canTransfer`, `data.findings` and
`data.status: "checked"` or `"attention"`. Exit 0 means the requested preflight passed;
exit 1 means a blocker, unavailable evidence, or a missing project needing approval.
Read-only failures use `outcome: "failed"`, not partial mutation. A pending/completed
handoff is previewed as a retry of its **original** snapshot/acknowledgement, never a
new capture of later edits; it does not replay setup or transfer follow-ups.

## Migrate a missing project

An ordinary transfer to a missing project returns a proposal without creating the
destination project/worktree. `--dry-run` produces the same proposal without writing
Graftr state on either machine. It separates portable setup instructions from the
destination's local settings, and prints an exact approval command:

```sh
graftr transfer my-project--fix-login --to laptop --dry-run
# Review the proposal, prepare any blocked local requirements, then rerun the preview.
# Run the printed command with --approve-project <the-current-64-character-hash>.
```

The hash approves this source recipe, proposed destination profile, destination
configuration and observed repository identity. Changed proposals are refused before
project creation. There is no implicit approval or `--yes` bypass. Source flat
profiles and the existing schema-2 destination configuration format are unchanged.

| Portable by default | Destination-owned, never copied from the source |
| --- | --- |
| Branch/path/label templates, public environment assignments, installation command/inputs/outputs, health checks, auto-setup and hooks | Main repository path, worktree root, environment baseline path/contents, local/HTTPS/named port ranges, public URL, global Herdr settings, Git remotes/config and resource identities |

Known source paths/addresses embedded in otherwise portable fields, and nonsystem
absolute command paths, require a destination replacement rather than silent
rewriting. This is conservative detection, **not a shell parser, secret scanner or
portability proof**. Approval imports trusted executable configuration, including
branch-controlled scripts it invokes. Privately review the source installer/hooks
for inline secrets, hostnames, nested commands and other local assumptions first.

Defaults suggest `$HOME/projects/PROJECT`, `$HOME/worktrees/PROJECT` and independent
100-port ranges from 32000 upward, avoiding other configured project ranges. Named
port names are retained, not their numeric ranges. These suggestions do not reserve
ports or prove listeners are absent. When the source manages `.env`, the proposed
baseline is beside the destination config at `environments/PROJECT/.env`; prepare
your approved development baseline there separately. Missing baselines block
approval. Public URLs are not guessed: provide a destination URL if the source uses one.

Override suggestions using a **private JSON file on the destination**:

```json
{
  "repo": "/home/you/projects/my-project",
  "worktreeRoot": "/home/you/worktrees/my-project",
  "localPorts": [42000, 42100],
  "envSource": "/home/you/.config/dev-environments/my-project/.env"
}
```

```sh
# The file must be owned by the destination user, mode 0600, not a symlink.
graftr transfer my-project--fix-login --to laptop --dry-run \
  --destination-config '~/.config/graftr/my-project-local.json'
```

The file is a profile fragment, not a `projects` wrapper; it may replace any project
field, including machine-specific hooks or public assignments. Quote `~/...` so it
expands on the destination, not the sender. Unknown fields, unsafe/noncanonical paths,
unrelated clones, existing task ownership and destination collisions are refused.
It is used **only for missing projects**; it cannot overwrite an existing profile.
Review the new proposal and use its printed approval command, which retains the
override path, selected extras and `--no-herdr` when supplied.

After approval, an existing matching main checkout is configured without changing
its files/index/remotes. Otherwise Graftr sends a checksum-verified, self-contained
bundle of the source main checkout's attached branch and clones it privately, then
adds only the new project to the destination configuration. Other projects and
top-level settings are preserved. A new clone requires a source main branch distinct
from the task branch and sharing its root history. Only its committed main-branch
history is seeded; unfinished task work then travels through the ordinary transfer.
Git hooks/templates, remotes, credentials, global Git configuration, dependency
directories and toolchains are not copied. Set up a Git remote separately if needed.

Project creation and worktree receipt are separate operations, not an atomic
transaction. Project bundles/packets and creation receipts remain private under
`state/project-transfers/PROJECT/APPROVAL/` on the receiver and
`state/transfers/PROJECT--TASK/project-APPROVAL/` on the sender; neither is auto-pruned.
An interruption can leave a new clone or a configured project even when transfer
fails. Preserve its receipt and inspect the configuration/repository; there is no
automatic rollback or replay of interrupted project creation. Once the project is
configured, retry the normal transfer **without** `--approve-project` or
`--destination-config`. They cannot retarget an existing project or recorded handoff.
Project main-branch bundles have their own 2 GiB limit.

## What travels

- The attached branch and its history, including commits that were never pushed.
- Staged and unstaged changes separately, including binary changes, deletions and
  renames. No WIP commit is added to your branch and the source index is not changed.
- Untracked files, except untracked `.env` and `.env.*` files at any depth.
- Git executable flags, symlinks themselves (not their targets), and literal working
  file bytes without applying clean/smudge or line-ending conversions.

Ignored files, dependency directories, caches, logs, external databases, running
processes, terminal/agent sessions and Graftr's machine-local registry do not travel
automatically. Untracked files can still contain secrets: review your checkout.

### Deliberately selected extras

```sh
graftr transfer . --to laptop --include notes.local --include .env.local
```

Each `--include` names one existing file relative to the checkout, not a directory
or glob. Selected files may be ignored. Selections are remembered for later handoffs
in either direction; a later deletion travels too. New selections can be added, but
an incoming file will not overwrite an unselected ignored file on the destination.
Symlinked parents, traversal, Git metadata, devices and sockets are refused.

Extra contents stay in private transfer storage, outside the project's Git object
database and refs. Restored regular extras are owner-private, retaining the owner
executable bit when applicable. Bundles are sensitive plaintext: SSH encrypts the
connection, not these local recovery files. Do not publish them or copy them into
Git. Source extras are never deleted.

Prefer the destination's baseline for `.env`. If explicitly selected, its bytes are
preserved rather than silently rewritten. A source-machine `PORT`, URL or other
managed assignment can therefore fail destination environment verification. Work
has been received, but setup is reported as needing attention; privately review
the destination file and explicitly retry setup. No values appear in transfer output.

## Existing folders and return safeguards

Managed task targeting works as usual. From an unmanaged linked worktree, use an
explicit path or omit the target inside it. Its basename must be a valid Graftr
task name. Adoption records can also be selected. Sending does not enroll or change
the source's external setup. A new destination becomes a normal provisioned task;
returning to an adopted/unmanaged source preserves that source's external setup
without running its installers/hooks or claiming its resources.

A new destination needs an unused task/path/branch. Existing checkouts, retained
branches, conflicting task records, frozen reservations and unrelated adopted
resources are not automatically adopted or overwritten. A return uses the exact
recorded original path, even if naming templates changed. It requires the matching
handoff lineage and an unchanged source HEAD, staged tree and portable working files,
including the previously selected extras. Machine-local ignored files not selected
for transfer remain untouched.

Restoration compares literal file contents, Git executable flags and file types,
and only writes changed files. Equal files keep their inodes and modification times;
the complete checkout is still captured and validated before a return. Selected
private extras may be replaced even with equal bytes to enforce private permissions
or avoid changing permissions through a hard link to another file.

Stop editors, servers and other file writers during capture/restoration. Graftr
locks serialize Graftr operations, and the destination Git index is locked during
restoration; these are not an atomic snapshot or sandbox against arbitrary writers.
An independently edited copy requires manual reconciliation before a safe handoff.
The second source capture uses its own fresh index and independently checks working
file contents. It can borrow already-written immutable objects from the first
capture's private store read-only, avoiding duplicate compression/storage without
using the first capture's cached file metadata. Newly observed objects stay in the
second private store, never the project Git database. The exported full bundle does
not depend on the verification store. When the working and staged trees already
match, verification uses the ordinary object lookup path instead; both captures
still run independently.

## Retries, output and recovery

```sh
graftr transfer fix-login --to laptop --json
```

`data.status` is `ready` or `attention`. Only `ready` means receipt and requested
setup completed. Setup/attachment failures return a partial public result with the
received path and source-retained indication. An SSH failure can leave receipt
uncertain even when the other machine finished; neither copy is deleted.

Completed/attention results include optional `data.timings.elapsedMs` and
`data.timings.stagesMs`, measured with a monotonic clock. `side` identifies sender
or receiver measurements (also when an older sender forwards receiver timings).
Sender stages cover the
destination check, capture, verification, bundle creation, packet writing and
`send-transfer` (SSH, receipt and destination preparation), plus optional
`run-after-transfer-hook` commands. When provided by the
receiver, `data.timings.destination` separates payload receipt, unpacking, history
import, checkout creation, destination capture, recovery preservation, restoration
and setup. Destination durations are inside `send-transfer`, not additional time
to add to it. Stages not run are omitted; coordination/locking overhead can remain
outside named stages. Operation totals exclude CLI startup/output and final
acknowledgement bookkeeping. Human output shows a rounded recorded timing summary.

Timed progress steps also contain `elapsedMs`, including `uncertain` failed steps.
An exception keeps the usual null `data`; inspect its timed progress instead. Other
commands' progress format is unchanged. Older peers/acknowledgements may omit
timings, and retries can omit capture/packing stages when reusing a saved snapshot.

Repeat the **same transfer command** to confirm a lost acknowledgement. A pending
retry sends the original immutable snapshot, not edits made afterward. A completed
retry reports its saved acknowledgement, even if setup was subsequently repaired.
Its saved timings describe the recorded attempt, not a new installation; after a
lost response, destination timings can come from an earlier successful receipt.
Destination installers/hooks are not replayed by retrying a transfer. Review their
logs/effects, then explicitly run `graftr setup --wait` or `graftr open TARGET` there.

Private version-1 receipts and self-contained recovery bundles live under
`$XDG_STATE_HOME/graftr/transfers/PROJECT--TASK/`. The current `receipt.json` names
the checkout, direction, generation and restoration/setup status. Original outbound
snapshots and received bundles remain available; a return also captures the prior
destination snapshot before replacing files. When it matches the saved outgoing
snapshot, Graftr preserves an independent, checksum-verified copy of that existing
full bundle rather than repacking history. Missing/corrupt cached bundle bytes are
regenerated from the verified checkout before restoration; unsafe file types or
permissions are refused. Recovery stays self-contained at the same
`received-GENERATION/before/work.bundle` path. Graftr does not automatically prune
these files or add snapshot branches/refs to your repository.

A `restoring` receipt without an acknowledgement means restoration may have stopped
mid-operation. Preserve both checkouts, the receipt and all bundles; inspect them
privately before any further Git/filesystem edits. There is deliberately no automatic
rollback, forced deletion, hook replay, or overwrite of uncertain state. This first
version has no automatic repair command for interrupted restoration. A `restored`
receipt means file restoration completed but setup may need explicit inspection and
retry; the checkout can be handed back after reviewing its state.

## Configurable follow-up commands

An optional sender-side project hook can hand off external artifacts separately:

```json
"hooks": {
  "afterTransfer": [
    {
      "name": "planboard",
      "command": [
        "sh", "-c",
        "exec plannr transfer --to \"$GRAFTR_TRANSFER_DESTINATION\" --worktree \"$GRAFTR_TRANSFER_DESTINATION_PATH\" --transfer-id \"$GRAFTR_TRANSFER_ID\" --lineage \"$GRAFTR_TRANSFER_LINEAGE\" --if-present --json"
      ],
      "timeoutMs": 600000
    }
  ]
}
```

This is a profile fragment calling a Plannr build with `transfer` support directly,
not a helper bin or built-in Graftr integration. It runs in the sending checkout only after the
destination confirms `ready`, with source project/task/branch and
`GRAFTR_TRANSFER_DESTINATION`, `GRAFTR_TRANSFER_DESTINATION_PATH`,
`GRAFTR_TRANSFER_ID` and `GRAFTR_TRANSFER_LINEAGE`. The command owns any additional
SSH and external data/binding transfer. `--if-present` skips missing branch boards
without creating one; project boards stay put. Both machines need that Plannr CLI
on their noninteractive `PATH`. Graftr itself does not copy planboards or
alter its Git payload. Configure it on each machine that should run it when sending.

These follow-ups are separate from setup hooks, including on external-source
returns. Failed/interrupted follow-ups keep the ready destination and report
partial completion on the sender. Optional JSON `data.afterTransfer` records
hook observations; `data.destinationStatus` distinguishes the ready destination
from sender follow-up attention. Ordinary cached retries never replay commands or
pick up newly configured ones. Review sender receipt/logs, fix the integration,
then run `graftr transfer TARGET --retry-hooks` to resume only unfinished follow-ups,
skipping recorded successes and reusing the original transfer context/Plannr ID.
No worktree files or setup are replayed; prior attempt evidence is kept privately.
The warning/JSON `retryCommand` names this recovery action. Setup/open is not a
follow-up retry. Destination `attention` skips
this phase, even if setup is repaired later. See [transfer hook context and recovery](hooks.md#transfer-follow-ups).

## Initial limits

Transfers use full self-contained Git bundles, currently at most 2 GiB, 100,000 files,
and 128 explicitly selected extra files. UTF-8 paths are required. Submodules,
shallow repositories, sparse/skip-worktree or assume-unchanged entries, intent-to-add
entries, unresolved conflicts and in-progress merge/rebase/cherry-pick operations
must be resolved first. File/directory replacements that collide with existing
directories or symlinked parents stop for review. Toolchains, global Git filter
configuration and external application data must be prepared separately.
