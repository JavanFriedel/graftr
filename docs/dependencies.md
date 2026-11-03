# Dependencies and public-source boundary

Graftr's own code is MIT licensed. Rust dependencies retain their respective licenses;
the project license does not replace them. `Cargo.lock` pins the complete graph.
The initial locked metadata contains 76 packages, including Graftr itself. Its
dependency license expressions use MIT, Apache-2.0, Unicode-3.0, Zlib and Unlicense
(including alternatives/combinations). No dependencies are vendored in this repository.
Review and retain required notices when distributing compiled releases.

Twelve direct crates provide:

| Crate(s) | Purpose |
| --- | --- |
| serde, serde_json | Validated records, configuration and JSON communication |
| chrono | Dates and timestamps |
| libc | Unix IDs, terminal/system calls and native locking |
| regex, url | Validation and URL parsing |
| sha2 | Stable identifiers and dependency fingerprints |
| toml | Plugin-manifest validation |
| unicode-segmentation, unicode-width | Terminal wrapping and display widths |
| icu_collator, icu_properties | Unicode-aware sorting and character properties |

Runtime executables are separate from these compiled dependencies: Git is required;
Herdr, systemd, Tailscale, database clients and project-specific installers are optional
and used only for their configured features. Linux/Unix support is intentional.

This repository is a fresh native-source extraction. It includes no former Git
history, private profiles, `.env` files, task registries, production logs, machine
addresses, local benchmark results or personal research reports. The plugin ID
`local.graftr` is deliberately stable for compatibility, not an account-specific ID.
HOME/XDG resolution and the legacy Devtree migration are user-independent behavior.

The previous installation/source repository is not a dependency. Existing deployments
can continue using their installed version until explicitly upgraded. Changing a
source checkout does not migrate absolute worker paths saved in existing timer units.
