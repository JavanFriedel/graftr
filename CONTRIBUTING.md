# Contributing

Keep changes small and preserve ownership checks, private file validation, native
locking, bounded subprocess/RPC behavior and partial-failure evidence. Do not weaken
tests to accept unsafe cleanup, uncertain external effects or stale-worker readiness.

Run the checks in the README. A public build must not require TypeScript, Bun or a
checkout outside this repository. Keep dependency versions locked and review license
and supply-chain implications before adding a crate. Update `help-catalog.json` and
documentation together for intentional CLI changes; review help snapshot diffs.

Integration tests use disposable repositories, synthetic environments and fake
service boundaries. Never use contributors' live workspaces, profiles, routes,
databases or systemd units as test fixtures. Real integration trials need their own
explicitly scoped disposable resources and cleanup.
