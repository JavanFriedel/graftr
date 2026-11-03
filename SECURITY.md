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
