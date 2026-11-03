# Internals and verification

Part of the [Graftr guide](../README.md).

`src/main.rs` dispatches one-shot commands. Separate modules handle validated records,
target resolution, setup, dependency fingerprints, environment planning, Git lifecycle,
Herdr transport/events, retained cleanup/retirement plans and scheduled return.
`help-catalog.json`, completion and the plugin manifest are embedded at compilation.

Private task records live under the user's XDG state directory. Setup receipts are
separate from task ownership and retain the latest optional timing sample and install
fingerprint. Adoption records explicitly preserve external setup rather than claiming
service ownership. Frozen recovery records reserve their branch/checkout identity.
Records do not store server PIDs or make Graftr a process supervisor.

State and worktree operations use native retained file locks. Private file readers
validate ownership/modes/types and recheck opened descriptors. Writes use staged
private files and atomic replacement. These controls do not make a multi-system
operation transactional: Git, filesystem, Herdr and trusted hooks can fail separately.
Progress distinguishes completed requests from uncertain effects; records and leases
remain when cleanup or ownership is not proven.

Setup work is outside the shared registry lock and serialized for a checkout. Saved
attempt identities fence stale workers so closure/removal cannot be undone by a late
installer completion. An active installer can still finish its own effects; fencing
is not arbitrary process termination. Cleanup runs in private scratch space outside
the checkout and skips already successful commands on an explicit retry. Failed or
interrupted external effects are never blindly replayed by automatic event workers.

Herdr commands use explicit socket/workspace/pane context, not another client's focus.
RPCs and owned subprocesses have finite deadlines. Applications remain user-managed.
Optional health commands/probes are separate from setup readiness. Scheduled returns
use one persistent one-off timer per task generation, saved absolute worker paths,
and bounded backoff when the recorded server/parent workspace is unavailable.

The regression suite uses real disposable Git/filesystem/process fixtures and fake
Herdr/Tailscale/Postgres/systemd boundaries. Golden help captures cover terminal widths;
build tests cover atomic replacement and failure preservation. Optional reference CLI
comparisons are distinct from default native repeatability tests. These checks do not
prove a real reboot, external-service outage, cross-machine deployment or power-loss
recovery. Use separately scoped disposable resources for live integration trials.
