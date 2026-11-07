# Security boundaries

Graftr is not a sandbox. Only run trusted repositories, installers and hook commands.
Hooks can perform arbitrary actions with your user's permissions. Optional Tailscale
operator access and database privileges are explicit machine-local setup, not granted
by installing Graftr.

Keep private profiles, `.env` files, task state, logs and backups out of Git. State
directories are owner-private; records and generated environments use private file
modes and validated identities. Output/log metadata is not permission to publish logs:
configured commands can print credentials. `logs --latest` intentionally reads content.

Graftr refuses dirty/unowned deletion rather than forcing it, but removing/freezing a
clean checkout deletes ignored files such as credentials, dependency directories and
caches. Review the confirmation and save anything you need first. Git branches do not
recover ignored files. Operations spanning Git, Herdr, the filesystem and external
hooks are not one atomic transaction; preserve records/leases after partial failures.

Configuration and state resolve from HOME/XDG paths; no account, tailnet, repository
path or project name is built into the executable. Supported platform assumptions
(Linux/Unix APIs, optional systemd) are documented, not portable-platform guarantees.

Report suspected vulnerabilities privately through this repository's GitHub security
advisory reporting if enabled. Otherwise contact the maintainer through their GitHub
profile to arrange private disclosure. Do not post credentials, private environments
or raw sensitive logs in public issues.

SSH handoffs use existing authentication and strict host-key checking; Graftr never
enrolls a host or forwards an agent. Transfer bundles/receipts are private plaintext
and may contain uncommitted code or explicitly included secrets. Ignored extras
stay outside project Git objects/refs and are restored owner-private. Do not publish
transfer storage. Returning work requires an unchanged recorded source snapshot;
independent changes and uncertain restoration are not forcibly overwritten. Review
the destination's trusted project configuration before running its installer/hooks.
Optional `afterTransfer` commands run from the sender's approved local profile only
after confirmed destination readiness. They own any further SSH/data handoff; Graftr
does not sandbox them or extend its transfer guarantees to external artifacts.
Failed/interrupted follow-ups retain the handoff and are never automatically replayed.
Explicit `transfer --retry-hooks` uses the recorded destination/generation, skips
recorded successes and preserves prior attempt results. Inspect external effects
before choosing it; approved current commands can have arbitrary effects again.

`transfer --dry-run` runs read-only Git/SSH inspections, not installers, hooks,
health checks or persistent Graftr writes. It is a point-in-time preflight, not a
snapshot lock or proof that executable setup will succeed. Missing-project migration
requires the current proposal's explicit `--approve-project` hash; existing projects
are not overwritten. Paths, port ranges, public URL and environment baseline are
destination-owned. Private destination override files must be owned regular files,
not symlinks. Environment contents, Git remotes/config, task registries and service
identities are not imported. Portable installer/hooks are still trusted code: obvious
embedded local paths are flagged, but arbitrary shell code, inline secrets and nested
tools cannot be classified reliably. Review them before approval. Project creation
is separately receipted and not atomic with worktree transfer; preserve private
project-transfer bundles/receipts after failure rather than forcing rollback/replay.
