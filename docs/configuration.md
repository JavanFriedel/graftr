# Project configuration

Part of the [Graftr guide](../README.md).

## Project configuration reference

The top-level `schemaVersion` must be `2`. `projects` maps project names (lowercase
letters, numbers, and single hyphens, at most 48 characters) to independent profiles.
Unknown profile fields are rejected. Optional fields should be omitted, not set to
`null`. The retired `originsKey` and `setupHooks` fields are refused with a request to
run `graftr migrate`.

The top-level `startupThaw` setting defaults to `true`. The existing Graftr Herdr
plugin reconciles owned workspaces and checks overdue scheduled returns at server
startup/handoff. Set `"startupThaw": false` alongside `schemaVersion` and `projects`
to skip only the startup return check. It must be a boolean, not a project-profile
field or an install option. Reconciliation, explicit `graftr thaw --due`, and
per-task systemd timers are unchanged. Top-level `"herdr": "off"` also suppresses
startup thaws. Unscheduled/future frozen tasks never return automatically at startup.

| Field | Required/default | Purpose |
| --- | --- | --- |
| `repo` | Required | Absolute canonical path to the main Git checkout. |
| `worktreeRoot` | Required | Absolute canonical base directory for CLI-created worktrees; native Herdr paths are retained. |
| `branchName` | `"task/{{task}}"` | Template for a new CLI branch; explicit `--branch` uses an existing local branch instead. |
| `worktreePath` | `"{{task}}"` | Relative CLI checkout path template beneath `worktreeRoot`; optional subdirectories are allowed. |
| `localPorts` | Required | Half-open `[start, end]` range for local app ports. |
| `httpsPorts` | Omitted | Half-open `[start, end]` secondary port range, for example the Tailscale HTTPS port. Omission selects local-only allocation. |
| `ports` | Omitted | Up to 16 extra named local port ranges, e.g. `{ "api": [3600, 3700] }`; each task leases one port per name. Names use lowercase letters, numbers and single hyphens (32 characters). |
| `url` | Omitted | Public HTTP(S) origin template. It declares an address; publishing is up to hooks. |
| `workspaceLabel` | Secondary port, else local port, then task name | Template for the managed Herdr workspace's display name. |
| `envSource` | Omitted | Approved development baseline file named `.env`; copied privately into new worktrees. Omission disables dotenv management. |
| `envOverrides` | `{ "PORT": "{{localPort}}" }` | Public environment assignment templates, replacing rather than merging the default; `{}` disables generated assignments. |
| `autoSetup` | `false` | Enroll new Herdr-native worktrees automatically. Explicit CLI setup does not require this flag. |
| `installCommand` | Omitted | Approved one-shot installer as an argv array, such as `["pnpm", "install", "--frozen-lockfile"]`; no command means manual installation. |
| `installInputs` | Omitted | Extra literal relative file paths to fingerprint in addition to automatic dependency inputs. |
| `installOutputs` | Automatic discovery | Literal relative files/directories required for installation reuse; an explicit array replaces discovery, and `[]` means fingerprint-only reuse. |
| `healthChecks` | Omitted/disabled | Up to 16 named HTTP, TCP, or command checks, executed only by `graftr health`. |
| `hooks` | Omitted/disabled | Ordered trusted commands in optional `afterEnvironment`, `afterDependencies`, `cleanup`, and `retire` phases, such as `graftr hook tailscale`. |

Port ranges start at unprivileged ports (1024 or higher) and exclude their upper bound.
Allocation selects distinct available ports from each range independently; the secondary
range is optional. Existing leases stay fixed when ranges change. Installation paths must
remain inside the checkout, without globs, traversal, or dotenv paths.

Health checks share `name`, `type`, and optional `timeoutMs` (default `2000`, range
`100`–`30000`). HTTP checks require a local request `path` and optionally
`expectedStatus` (default `200`); TCP checks have no additional fields; command checks
require a `command` argv array. See [Optional application health](everyday-use.md#optional-application-health) for execution details.

## Creation naming templates

Set these fields per project to change **new CLI** checkouts while retaining the
existing task IDs and independent workspace-label format:

```json
{
  "worktreeRoot": "/home/you/worktrees",
  "branchName": "feature/{{task}}",
  "worktreePath": "{{project}}/{{branchSlug}}",
  "workspaceLabel": "{{httpsPort}} {{taskName}}"
}
```

This is a profile fragment; keep the required repository, port, and environment
settings from the full example. `graftr create my-project fix-login` creates branch
`feature/fix-login` at `/home/you/worktrees/my-project/feature-fix-login`. The task ID is
still `my-project--fix-login` and the workspace label is still, for example, `5200 fix login`.
Omit either field to keep its default. No active profile is changed automatically.

| Placeholder | `branchName` | `worktreePath` | Value |
| --- | --- | --- | --- |
| `{{task}}` | Yes | Yes | Validated short task argument, such as `fix-login`; not the human-readable label's `taskName`. |
| `{{project}}` | Yes | Yes | Project profile name, such as `my-project`. |
| `{{repoName}}` | Yes | Yes | Basename of the configured main checkout path, not a remote repository name. |
| `{{branch}}` | No | Yes | Actual full branch, including slashes, after applying `--branch` or `branchName`. |
| `{{branchSlug}}` | No | Yes | Actual branch flattened for a folder name: preserve ASCII letters/digits/dots/underscores/hyphens, replace other runs with `-`, collapse repeated hyphens, and trim edge dots/hyphens. Case is retained. |

Whitespace inside placeholders is allowed; unknown or malformed placeholders are
errors. Templates are nonempty text strings of at most 4096 characters without
control characters or leading/trailing whitespace. There are no expressions, shell
commands, environment-variable lookups, glob expansion, or recursive substitutions.
`branchName` cannot refer to its own result or to the resulting path. Ports, host,
URL, workspace, and label-only `taskName` values are unavailable before creation.
Literal text such as `$USER` stays literal; use supported placeholders instead.
Rendered branch names are checked as exact local Git branches, not revision expressions.

`worktreePath` is a **relative, normalized** path beneath `worktreeRoot`, not another
absolute root. For nested branch directories use `"{{branch}}"`; to flatten them use
`"{{branchSlug}}"`. Absolute paths, `.`/`..` traversal, repeated/trailing separators,
backslashes, Git metadata components (`.git`, case-insensitive), control characters,
and components over 255 UTF-8 bytes are rejected. The final absolute path must fit
Linux's 4095-byte usable path limit and have canonical, nonsymlinked parents/destination.
Graftr rechecks canonicality after parent-workspace discovery before creation.
These are safety checks, not an atomic filesystem sandbox against concurrent actors.

Existing folders are never adopted/overwritten by create. Retained managed records
(even for missing folders) and this repository's Git worktree registrations reserve
their paths; nesting a new checkout inside another linked checkout or enclosing one
is refused. A configured root inside the main checkout is still supported. Normal
Git branch collisions and flattened-name collisions fail rather than inventing a
counter, random suffix, new branch, or hidden replacement. `branchSlug` is deliberately
lossy, **not a unique identifier**; use `{{task}}` or a different layout when needed.

`--branch NAME` bypasses branch-name rendering and checks out that existing, unused
local branch. The path template receives its **actual** branch/slug, not the would-be
branchName result; template syntax is still validated. The configured branch/path
also apply with `--no-setup`. Parsed explicit creation settings that are invalid, or
lack canonical repo/root paths, fail before parent association/creation rather than
silently using native names. Missing/unreadable legacy configuration still retains
its documented plain-checkout fallback. Incorrect Herdr paths/branches are left for
review, never silently enrolled under the requested task.

Changing templates or `worktreeRoot` does **not** move/rebranch existing records,
migrate IDs, reassign ports/URLs, rewrite environments, or by itself reinstall
dependencies. Repeated create and open use recorded paths/branches. Native Herdr
enrollment keeps the native path and branch; these fields do not configure Herdr's
own Create Worktree dialog. Workspace labels continue to follow `workspaceLabel`.
After remove, recreating with `--branch` uses the **current** path template and a
fresh environment/lease; ignored files are not restored. Removal deletes the checkout
only, leaving empty parent grouping directories in place. Rename/UUID migration and
automatic cleanup of parent directories are not part of creation naming.

## Workspace label templates

Set `workspaceLabel` independently in any project profile:

```json
"workspaceLabel": "{{httpsPort}} {{taskName}}"
```

Supported placeholders are:

| Placeholder | Value |
| --- | --- |
| `{{taskName}}` | Branch-derived task name: remove the first slash-separated namespace, replace dashes and remaining slashes with spaces. `feature/fix-login` becomes `fix login`. A branch without a slash keeps its whole name before replacing dashes. |
| `{{branch}}` | Full attached Git branch, such as `feature/fix-login`. |
| `{{project}}` | Project profile name, such as `my-project`. |
| `{{localPort}}` | Assigned local application port. |
| `{{httpsPort}}` | Assigned optional secondary port; unavailable if unconfigured. |
| `{{port.NAME}}` | Assigned named port from the profile's `ports`; unavailable if that name is not leased. |
| `{{url}}` | Full configured HTTP(S) origin; unavailable if unconfigured. |
| `{{host}}` | URL hostname without protocol or port; unavailable if unconfigured. |
| `{{worktreeName}}` | Checkout folder name, unchanged. |
| `{{worktreePath}}` | Absolute checkout path. |

For example, `"{{project}}: {{taskName}} [{{httpsPort}}]"` renders as
`my-project: fix login [5200]`. A literal string is also supported. Whitespace
inside placeholders is allowed. Templates and rendered labels must be nonempty,
at most 4096 characters, with no control characters or leading/trailing whitespace.
Unknown and malformed placeholders are errors. No expressions, shell commands, or
environment lookups are evaluated; label templates should contain public metadata only.

Label changes take effect on the next setup/open, including before queued setup
finishes. Removing the setting restores the default. They do not change task IDs,
branches, folders, ports, URLs, environment files, or dependency fingerprints, and do
not require closing a running workspace. Labels are read from the current profile,
not persisted into task records. Plain/unmanaged worktrees keep their native labels.
`taskName` is a label-only placeholder; environment placeholders are listed separately
below.

## Environment and installation

Paths must be absolute and canonical. `envSource` is optional and has no default path.
Omit it for projects that do not need a dotenv file: Graftr does not copy, create,
read, or verify a worktree `.env`, even if one already exists. An explicit `null` or
empty string is invalid. When supplied, `envSource` is a file path with basename `.env`,
not a directory; it can point outside the repository, such as
`/home/you/.config/dev-environments/my-project/.env`. The target is always the worktree-root
`.env`. Approve a **development** baseline; never point at production credentials.
Removing `envSource` never deletes an existing file. Public `envOverrides` still reach
approved installers and newly created managed shell tabs. Without a dotenv source,
Graftr creates all four named tabs with those process overrides and leaves the initial
native shell untouched rather than injecting exports into a running terminal. That
original shell is not a configured Graftr tab; use the named agent/editor/terminal/server
tabs for the worktree environment. Existing tabs retain their exports on retries.
`autoSetup` defaults to false and `installCommand` is
optional; existing version-2 profiles remain valid without them. Approve the installer
once per project: install scripts run trusted checkout code as your user and can read the
copied environment. No server commands or credentials belong in the profile. Required
project tooling (for example pnpm/Node) must also be available to Herdr's runner, even
when graftr itself is compiled. Environment verification uses Graftr's native
dotenv reader before installation; no JavaScript runtime is involved.
Neither the parent repository nor the new checkout needs `dotenv-cli`, `dotenv`,
`node_modules`, or a JavaScript package manifest. Verification parses the file without
expanding variable references or loading values into Graftr's process environment;
it does not load the application's parser or validate application-specific env expansion.

## Project-specific environment assignments

An optional `envOverrides` map replaces the generated assignment list completely.
It is **not merged** with Graftr's defaults; baseline values not named in the map
are still copied unchanged. For example, a project's profile can include:

```json
"envOverrides": {
  "PORT": "{{localPort}}",
  "VITE_APP_URL": "{{url}}",
  "ALLOWED_DEV_ORIGINS": "http://localhost:{{localPort}},{{url}}",
  "WORKTREE_NAME": "{{worktreeName}}",
  "NODE_ENV": "development"
}
```

Omitting the map generates only `PORT`. An explicit empty map (`"envOverrides": {}`)
copies the baseline without generated assignments. Every task record stores the map it
was prepared with.
It does **not** automatically adapt the application's listening port: include the
variables your application needs to use its assigned local port and URL.

Supported placeholders are `{{localPort}}`, `{{httpsPort}}`, `{{port.NAME}}`, `{{url}}`, `{{host}}`,
`{{worktreeName}}` (checkout folder name), `{{worktreePath}}` (absolute checkout path),
`{{branch}}`, `{{project}}` (profile name), and `{{taskSlug}}` (see `GRAFTR_TASK_SLUG`). The branch is the actual attached Git
branch, including Herdr-native branch names. Spaces inside a placeholder are allowed.
Values can also be literal strings, including empty strings. There is no shell,
expression evaluation, environment lookup, or secret interpolation.

Assignments are **public metadata**: they appear in task records/status and are passed
to approved installers and newly created shell tabs. Keep credentials in the private
baseline, never in this map. Validation rejects common secret-bearing variable names,
process-control variables (including `PATH`, `NODE_OPTIONS`, and Herdr/XDG/Git settings),
unknown/malformed placeholders, and values containing control characters, quoting,
shell expansion, backslashes, comments, or leading/trailing whitespace. Names must be
ordinary environment identifiers, at most 128 characters; maps support at most 64
string assignments of at most 4,096 characters each. Validation cannot identify every
secret hidden in an otherwise ordinary literal; choosing non-secret values remains
your responsibility. Invalid maps leave newly created worktrees usable through the
same plain-checkout fallback as other invalid profile settings.

Changing the map does not refresh existing files or shell exports. With `envSource`
configured, setup/open verifies the desired assignments against the worktree's existing
`.env`. If they differ, setup stops with the mismatched **variable names**, not values; the
file and previously recorded overrides stay untouched. Review/edit that worktree's
`.env` yourself, then run `graftr setup --wait` there or retry its Herdr setup action.
Only successful verification adopts the new declared/resolved map into its record.
Without `envSource`, there is no dotenv verification step: validated public assignments
are adopted directly, but existing files and shell exports are still not rewritten.
Absent files can be generated from the current profile. Removing a key from the map
stops managing it but does **not** remove the assignment from an existing file; review
and remove it manually if needed. Retained tabs are not replaced or injected into:
reload the environment or use a fresh shell if its exports are stale.

Verified changes to resolved assignments invalidate the prior install receipt, so the
same approved installer runs with the new overrides unless `--skip-install` is active.
The environment map is checked again before readiness; a map change during
setup cannot silently publish stale settings. Ports, URLs, branches, and folder names
are not reassigned by changing the map.

### Planboard: no dotenv file

Planboard uses its YAML runtime settings and process overrides, not a required `.env`.
Its Graftr profile therefore omits `envSource` and uses explicit public assignments:

```json
"envOverrides": {
  "PLANBOARD_HOST": "127.0.0.1",
  "PLANBOARD_PORT": "{{localPort}}",
  "PLANBOARD_BASE_URL": "{{url}}",
  "PLANBOARD_DATA_HOME": "/home/you/.local/share/planboard-dev/{{worktreeName}}",
  "PLANBOARD_CONFIG_PATH": "/home/you/.config/planboard-dev/{{worktreeName}}.yaml"
}
```

Use your actual absolute home path. The per-worktree data and settings paths isolate
development from live boards. Graftr does not author the application's YAML; Planboard
initializes its settings on first use, while the explicit data override selects the
isolated directory. Keep agents' real task boards separate from that disposable app data.
No servers start during Graftr setup. Planboard's current `bun dev` still uses internal
API port 7332, so simultaneous HMR stacks need separate API-port support; isolated tests
and production-style `planboard serve` instances can use their assigned ports normally.

### my-project: localhost and private Tailscale access

my-project's development profile preserves the localhost auth configuration used by the
older numbered worktrees while allowing its assigned private Tailscale HTTPS origin:

```json
"envOverrides": {
  "PORT": "{{localPort}}",
  "BASE_URL": "http://localhost:{{localPort}}",
  "AUTH_URL": "http://localhost:{{localPort}}",
  "NEXT_ALLOWED_DEV_ORIGINS": "http://localhost:{{localPort}},{{url}}"
}
```

This replaces Graftr's generic HTTPS assignments; it does not change the public
task URL or the HTTPS proxy to the local server. my-project's browser API/auth requests
use the browser's current origin, and auth trusts both declared origins. `BASE_URL`
controls server auth defaults, including cookie naming and generated absolute URLs;
`AUTH_URL` selects the agent-browser helper's default origin, not the auth server's.

Keeping `BASE_URL` on HTTP retains `better-auth.session_token` rather than changing
to `__Secure-better-auth.session_token`. With a shared development database, auth
secret, and cookie prefix, a login can be reused across ports on the same hostname.
Localhost and the Tailscale hostname still need separate browser cookies/logins.
This intentionally retains the non-Secure development cookie policy for local/private
Tailscale access only; do not use it for production. HTTPS transport is unchanged.
Generated links and Microsoft OAuth callbacks still follow `BASE_URL`; this is not
a per-request origin solution for those flows.

For an existing worktree, review/update only `BASE_URL` and `AUTH_URL` in its final
managed `.env` block to `http://localhost:<assigned-local-port>`. Keep `PORT` and
the localhost/Tailscale allowed-origins list unchanged. From that worktree's Herdr
shell, run `graftr setup --skip-install --wait` to verify/adopt the reviewed settings
without closing the workspace or reinstalling dependencies. This persists the install
skip until `graftr setup --install` is explicitly requested. Setup does not replace
existing shell exports or interrupt running servers. At a convenient point, restart
the server with refreshed exports (for example, `unset BASE_URL AUTH_URL` before
the usual development command, which loads `.env`). Do not close/recreate the worktree.
