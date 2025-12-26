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
   - `crates/unity-asset-search-core/Cargo.toml`
   - `crates/unity-asset-search-index/Cargo.toml`
   - `apps/unity-asset-search-daemon/Cargo.toml`
   - `apps/unity-asset-search-cli/Cargo.toml`
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
   4) `unity-asset-search-core`
   5) `unity-asset-search-index`
   6) `unity-asset-decode`
   7) `unity-asset`
   8) `unity-asset-cli`
   9) `unity-asset-search-daemon`
   10) `unity-asset-search-cli`
4. Builds and uploads multi-platform binaries using `cargo-dist`:
   - `unity-asset-search-daemon` (for UnityHero)
   - `unity-asset-search-cli` (debug/ops utility)
5. Creates a GitHub Release using release notes extracted from `CHANGELOG.md` and attaches the built binaries.

## Backfill missing dist assets (existing tag)

If a tag already exists and the GitHub Release was created without dist assets (e.g. early release workflow),
use the manual workflow:

- GitHub Actions → `Upload cargo-dist assets to an existing tag`

Inputs:

- `tag`: the existing tag (e.g. `v0.2.0`)
- `ref`: the git ref to build from
  - use `main` to backfill old tags (note: this trades exact reproducibility for a practical repair)

## UnityHero packaging (scheme B)

UnityHero (UPM plugin) vendors the daemon binaries into:

- `Packages/com.frankorz.unityhero/Tools/<platform>/`

The UnityHero release workflow should:

1. Download `unity-asset-search-daemon` archives from this repo's GitHub Release.
2. Extract and place them into `Tools/win-x64/`, `Tools/linux-x64/`, and `Tools/mac-universal/`.
3. For macOS, merge `x86_64` + `aarch64` into a universal binary (e.g. `lipo -create`).
4. Ensure macOS/Linux binaries are executable (`chmod +x`).
