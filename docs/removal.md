# Removing tasks

Part of the [Graftr guide](../README.md).

## Remove a finished task

`close` is reversible; `remove` is full retirement. Run **`graftr remove` inside the
managed worktree**, including a nested directory or plain terminal, or specify
`graftr remove PROJECT--TASK` from elsewhere. Bare removal first matches the canonical
current folder against enrolled records; otherwise it uses the explicit caller's Herdr
workspace. It never guesses from another client's focus. Native plugin actions use
their selected workspace, not the plugin's own folder.

The CLI preflights the target, then shows the task, folder, original recorded branch,
current attached branch/commit, and warning
that the whole folder (including ignored `.env`, dependencies and caches) will be
deleted. Only `y`/`yes` confirms; Enter, another answer, EOF or Ctrl+C cancels without
running cleanup or changing the workspace or files. Noninteractive invocations refuse rather than
accepting piped answers. After reviewing the target, automation can explicitly use
`graftr remove PROJECT--TASK --yes`; this skips only confirmation, never safety checks.
Existing explicit Herdr delete actions run noninteractively without a terminal prompt.

For an attached task, when the caller is in the target directory **or** its Herdr workspace, the confirmed
request hands off to Herdr's independent `local.graftr` removal action. The plugin
must be linked/enabled and use the same XDG configuration/state as the caller. The
worker checks the confirmed task/folder/repository/workspace identity again, including
the observed current branch and commit,
then completes cleanup outside the terminal it closes. A failed handoff never falls
back to deleting its caller. From inside a checkout without its owned Herdr workspace,
run removal from outside with an explicit ID instead. Unattached standalone tasks
remove directly, including from inside the checkout, without a Herdr worker.

A `status: "removing"` receipt with a `logId` means the worker **accepted** the request,
not that deletion finished; its caller may close before printing that receipt. Inspect
`graftr list PROJECT`, `graftr status` and the Herdr plugin log from another workspace
if removal fails. A standalone terminal outside Herdr is not closed by Herdr; it may
remain in a deleted working directory after success, so `cd` elsewhere.

Removal only accepts an enrolled task with a canonical checkout path, matching Git
registration/repository, and an attached local branch whose HEAD is retained on that
branch in the same repository. That branch may differ from the original recorded
branch. The main checkout, symlink paths, locked/detached worktrees, duplicate ownership records,
and checkouts open in another unowned workspace are refused. Index flags such as
`assume-unchanged` and `skip-worktree` are refused too: they can hide edited files from
both Git status and ordinary worktree removal. Clear those flags deliberately first.

Confirmation binds both the historical record identity and observed branch/commit.
A branch switch or new commit after confirmation/dispatch requires a fresh preview
and confirmation. The independent worker cannot substitute a newly observed branch
for the one handed off. Branch/commit and dirty/index checks repeat around cleanup
hooks and workspace shutdown before Git removal. A late change can leave cleanup run
and the workspace closed while preserving checkout and leases for review.

Git status checks **staged, unstaged, and untracked files** before running cleanup or
closing the workspace. Commit or move that work first; graftr never auto-commits, stashes, or uses
`--force`. Git-ignored files are disposable: **the copied `.env`, including local edits,
`node_modules`, and ignored build/cache files are deleted with the folder**. Save any
ignored data you want to keep before removing.

For a clean task, removal runs its recorded cleanup hooks, closes only its owned Herdr
workspace, rechecks Git status in case pane shutdown saved new work, then runs
`git worktree remove` without force. Git performs its own final dirty/locked/submodule
checks. Clean initialized-submodule worktrees may still be refused; no-force protection
is deliberately retained. Detached/external servers must be stopped by you first;
graftr does not identify or signal them.

No Git branch is deleted, renamed, created, reset, or reattached by removal. The
original branch need not still exist if the user already deleted it; Graftr neither
restores it nor mistakes its absence for loss of the currently attached branch.
`nuke` does not gain this flexibility and still refuses a changed recorded branch.

The entire checkout and its Git worktree registration must be gone before the record
is deleted and its ports become available again. No copy of the record is kept.
All existing branches and their reachable commits remain in the main repository;
merge, push, and branch deletion are not required or performed. No profile/task
schema change is needed.

If a later check or Git removal fails, cleanup may have run and the workspace may be closed, but
the remaining files and lease record are preserved. Commit any newly saved work and
retry, or switch back to the recorded branch before `open` if preparation is needed.
If folder deletion succeeded but deleting the record failed,
rerunning `remove` finishes after confirming the folder/registration are gone
and ownership still matches. When both checkout and registration are already absent,
no last current branch is invented: removal results use `branch: null`, with the
original `recordedBranch` for context. Otherwise `branch` is the observed retained
branch; `recordedBranch` appears when different.
Missing directories with stale registrations can
also be removed safely through Git; no blanket `prune` or recursive filesystem deletion
is used. An unreachable recorded Herdr server is an error, not permission to delete
behind it. Unattached tasks do not query a server.

The helper's lock serializes its own operations, not external Git edits or file writes.
Stop editing/running commands against a checkout before disposal. After removal, `open`
cannot restore that task from its history; use `create --branch` to resume its retained
branch in a fresh checkout. Configured CLI create still refuses to adopt an existing
branch without that explicit option; the unconfigured native fallback follows Herdr's
normal branch handling.

## Permanently remove a merged task

```bash
graftr nuke my-project--fix-login
```

Nuke removes a managed clean worktree and its local branch, keeping remote branches
untouched. Run it in an interactive terminal **outside both the target folder and
its Herdr workspace**, with an explicit task ID. It displays the folder, branch tip,
and merge target, requires the exact branch name, then asks for a final yes/no
confirmation. Empty input, EOF, Ctrl+C, or a mismatch cancels. There is no `--yes`,
`--force`, plugin action, or noninteractive bypass.

The branch must be merged into the main checkout's current attached branch, which is
named in the prompt. Normally its commits are in that branch's history. After a squash
or rebase merge they are not; the branch then counts as merged **by content** only if
`git merge-tree` merges it into the main branch without conflicts and produces exactly the
main branch's tree, so merging it again would change nothing. The prompt says when this
applies. Patch similarity or a closed pull request is never treated as proof: a partial
squash, a later revert on main, or a conflict is refused. A content-merged branch is
deleted with a compare-and-delete of the confirmed tip (`git update-ref -d`), so a branch
that moved after confirmation is kept. A configured upstream must also contain the
branch's commits. `main`, `master`, the main checkout's branch, known remote-default
branches, and branches used by another checkout/task are protected. No fetch, push,
merge, or reset is performed.

All ordinary removal protections remain: verified canonical ownership, private state,
clean staged/unstaged/untracked files, no hidden index flags, no locks/detached HEAD,
no active setup, completed cleanup hooks, and Git's final no-force worktree safeguards.
Saved editor work is rechecked after pane shutdown. The branch tip and main-checkout
merge target are revalidated after confirmation and before each destructive stage;
changes require review and fresh confirmation. The final no-force branch deletion
checks the main checkout's HEAD rather than trusting a self-referential upstream,
without modifying stored Git configuration.

**Ignored files such as `.env`, dependencies, and caches are deleted.** There is no
automatic undo; use `remove` when you may want to resume the branch later.
Stop detached/external servers yourself. As with remove, this is not a transaction
across Herdr, Git, and disk. If a later step fails, cleanup, the workspace or checkout may
already be gone, but the record and leases stay until branch deletion and record deletion
complete. Fix the reported issue and run nuke again to review/reconfirm. A retry can
finish after the branch is already absent; a newly recreated branch cannot be deleted
using a stale confirmation.
