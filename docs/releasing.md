# Versioned binary releases

The release workflow currently builds Linux x86_64 GNU binaries on Ubuntu 22.04.
Other operating systems/architectures are not implied by a successful build.

1. Deliberately update Cargo's package version and lockfile when making a new version.
2. Review and commit a clean tree, then push the intended commit. Run the normal checks.
3. Create and push an explicit `vVERSION` tag matching Cargo's exact version. Never
   move a tag to replace an already published release.
4. The tag workflow runs the complete checks, builds a fresh native executable and
   creates a **draft** release. It does not silently publish it as latest.
5. Review/download its archive and `SHA256SUMS`; verify the archive, `RELEASE.json`,
   source revision and executable, then publish the draft deliberately.

`scripts/package-release.py --tag vVERSION` requires a clean committed checkout,
rebuilds the executable, and refuses to overwrite existing release assets. It uses
Python 3.10+, Cargo and the matching `rust-docs` license component as release tooling,
not runtime dependencies. The archive includes project/dependency/Rust-library notices,
the executable, completion and plugin manifest. Packaging sets the compile-time
`GRAFTR_SOURCE_REVISION` to the clean checkout's commit and requires
`graftr --version --json` to match that revision and Cargo's version before creating
an archive. Do not set this variable for ordinary dirty/unverified local builds.
`RELEASE.json` records source revision,
version, target and each payload file's SHA-256. No local paths, credentials, profiles,
task state or benchmark results belong in release artifacts.

Consumers should pin the exact archive URL and a reviewed SHA-256 in their own tool
manifest. A downloaded checksum beside an archive is useful for transfer integrity,
but is not an independent trust anchor; dotfiles pins its reviewed digest in Git.
Changing a release pin is a deliberate upgrade. Keep executable paths/version
directories required by saved timer workers instead of garbage-collecting them blindly.
