# Releasing

This repository uses a tag-driven release workflow.

## Prerequisites

- GitHub Actions `release.yml` enabled.
- `CARGO_REGISTRY_TOKEN` secret configured in the GitHub repo.

## Release steps

1. Decide the version (e.g. `0.2.1`).
2. Update versions in all workspace crates:
   - `crates/unity-asset-core/Cargo.toml`
   - `crates/unity-asset-yaml/Cargo.toml`
   - `crates/unity-asset-binary/Cargo.toml`
   - `crates/unity-asset-decode/Cargo.toml`
   - `crates/unity-asset/Cargo.toml` (published as `unity-asset`)
   - `apps/unity-asset-cli/Cargo.toml`
3. Ensure path dependency versions match the same version (the release workflow validates this).
4. Update `CHANGELOG.md`.
5. Run locally:
   - `cargo fmt --all`
   - `cargo clippy --workspace --all-targets -- -D warnings -A clippy::collapsible_if`
   - `cargo nextest run --workspace`
6. Commit changes.
7. Create and push a tag:
   - `git tag v0.2.1`
   - `git push origin v0.2.1`

## What CI does

On tag push (`vMAJOR.MINOR.PATCH`), GitHub Actions:

1. Validates that the tag version matches all crate versions and workspace path dependency versions.
2. Runs formatting, clippy, and tests.
3. Publishes crates to crates.io in dependency order:
   1) `unity-asset-core`
   2) `unity-asset-yaml`
   3) `unity-asset-binary`
   4) `unity-asset-decode`
   5) `unity-asset`
   6) `unity-asset-cli`
4. Creates a GitHub Release using release notes extracted from `CHANGELOG.md`.
