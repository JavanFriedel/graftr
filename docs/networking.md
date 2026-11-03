# Networking and ports

Part of the [Graftr guide](../README.md).

## Networking through hooks

Graftr assigns each task a local port and, when the profile declares `httpsPorts`, a
secondary port. It never publishes anything itself: a URL is only a declared `url`
template, and publishing or unpublishing is done by commands in the profile's `hooks`.
A local-only project needs no Tailscale installation, identity, permissions, URL
or secondary port range. For example, in machine-local `projects.json`:

```json
{
  "schemaVersion": 2,
  "herdr": "auto",
  "projects": {
    "my-app": {
      "repo": "/home/you/my-app",
      "worktreeRoot": "/home/you/worktrees/my-app",
      "localPorts": [3200, 3300],
      "envOverrides": { "PORT": "{{localPort}}" }
    }
  }
}
```

`create`, `setup`, `open`, `doctor --live`, `cleanup`, `close` and `remove` do not
contact Tailscale for these tasks, with or without Herdr. Unconfigured `httpsPort`,
`host` and `url` are `null` in task records/output; optional hook context variables
are omitted. Without `envOverrides`, profiles generate only `PORT`; `{}` disables
generated assignments. Start the app yourself and use localhost.

To publish, add a secondary `httpsPorts` range, an explicit HTTP(S) origin `url`
template, and a single `hooks` object with `afterEnvironment`, `afterDependencies`
and/or `cleanup` arrays. Commands are literal argv; explicit `sh -c` enables ordinary
shell expansion directly in the profile. See [Unified lifecycle hooks](hooks.md#unified-lifecycle-hooks).

For example, this profile fragment declares an address without publishing anything:

```json
{
  "httpsPorts": [5200, 5300],
  "url": "https://dev.example.test:{{httpsPort}}",
  "envOverrides": { "PORT": "{{localPort}}", "APP_URL": "{{url}}" },
  "hooks": {}
}
```

`url` must render to an HTTP(S) origin, without credentials/path/query/fragment;
`host` derives from that origin. It cannot discover a hostname or refer to itself.
Omitting `url` invents no URL, even when a secondary port is reserved. Referencing an
unconfigured address in environment/label templates fails rather than emitting `null`.
Default workspace labels use the secondary port when assigned, otherwise the local port.

The allocator excludes all retained Graftr leases and checks local listeners. When the
profile publishes with the built-in Tailscale hook it also skips every port that already
has Serve configuration (see [Port allocation](#port-allocation)). It does not inspect any other
publisher's remote inventory: such commands must verify ownership and collisions
themselves. An arbitrary `tailscale serve` one-liner does not reproduce the built-in
hook's route-ownership safeguards. Commands run as you, not in a sandbox.

## Built-in Tailscale publisher

`graftr hook tailscale` publishes a task's private tailnet URL in `afterDependencies`
and removes it again in `cleanup`. Substitute the machine's actual tailnet hostname:

```json
{
  "httpsPorts": [5200, 5300],
  "url": "https://your-machine.your-tailnet.ts.net:{{httpsPort}}",
  "hooks": {
    "afterDependencies": [
      { "name": "publish-tailscale", "command": ["graftr", "hook", "tailscale"] }
    ],
    "cleanup": [
      { "name": "cleanup-tailscale", "command": ["graftr", "hook", "tailscale"] }
    ]
  }
}
```

Keep the project's installer, environment and other settings alongside this fragment.
The hook needs Tailscale running with tailnet HTTPS and local operator access (see
[One-time Tailscale permission setup](setup.md#one-time-tailscale-permission-setup)), and `graftr` on the PATH of the Herdr worker
and of every shell that runs cleanup. An absolute path to the `graftr` executable also
works; allocation recognizes any command ending in `hook tailscale`.

URLs are always `https://<host>:<httpsPort>`. Serve terminates TLS and proxies to
`http://127.0.0.1:<localPort>`; that internal HTTP target does not make the published
URL HTTP. The hook reads its `GRAFTR_*` context, refuses to act when it no longer
matches the task record and cleanup generation, and checks the URL's host against the
local Tailscale identity before publishing. It verifies the exact HTTPS route before
recording it active and refuses to adopt or overwrite any existing route on that port.

Private per-generation receipts under the **resolved Graftr state directory's**
`tailscale-hooks/` record a pending intent before each Serve request, so a lost reply
still leaves exact cleanup ownership. An exact-looking route with no recorded ownership
is not adopted or deleted; cleanup before publication is a no-op. Cleanup disables only
the recorded port's exact single `/` proxy: changed or foreign handlers and Funnel
settings are preserved for review. Graftr never resets Serve or enables Funnel, and
external Serve edits are not serialized by its locks. Cleanup does not depend on the
deleted checkout. Keep receipts after uncertainty; do not delete them to force adoption.

Core task records carry no route state. Use full setup/open to publish and
cleanup/close/remove to unpublish. Doctor does not verify hook resources; inspect
`tailscale serve status --json` directly.

## Port allocation

Port ranges are half-open: my-project's `[3200, 3300]` and `[5200, 5300]` mean local ports
3200–3299 and HTTPS ports 5200–5299. Allocation takes the lowest local port and the
lowest secondary port that are different from each other and appear in no retained
Graftr lease (across every project and lifecycle state) and have no IPv4/IPv6 local
listener. Each range is scanned independently, so the pair need not share an offset.

Named `ports` are allocated after that pair, in profile order: each takes the lowest port
in its range that is not one of the task's other ports, not leased by any retained task
(named leases included) and has no local listener. The set of names is part of the pinned
address contract, like the secondary port: adding or removing a name refuses setup for
existing tasks, which keep their recorded ports; use a new task. Named ports are local
only; the built-in Tailscale hook still publishes the primary URL.

When the profile's `afterDependencies` hooks include `graftr hook tailscale`, ports with
any Serve configuration (including foreground routes and plain-HTTP or TCP entries) are
skipped too, because the hook would refuse to adopt them. Allocation then needs
`tailscale serve status`: if it cannot be read, the new checkout is kept unmanaged with
the reason, and `graftr setup` retries. Other publishers' inventories are not consulted.
Exhaustion is an error, never a silent fallback to random ports.

A machine-wide kernel lock briefly serializes enrollment, leases and tab bookkeeping,
never dependency installation or hook commands. A separate canonical-path
lock serializes setup per checkout; transient `flock` children release their locks on EOF,
including worker exit. No daemon or application-process watcher is added. Cleanup and closing retain leases across projects
so the same task reopens at the same URL. Existing unmanaged worktrees have no graftr
lease, although their live listeners (and Serve configuration, for the Tailscale hook) are checked during allocation.
No existing assignments are changed when editing the profile's ranges.

Leases are not held sockets: use the assigned port and handle later conflicts yourself;
the helper never inspects your running server. Each machine has separate paths, leases,
and Tailscale hostname. Do not copy these local identities to another machine.
