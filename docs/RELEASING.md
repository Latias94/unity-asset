# Releasing

This repository uses a tag-driven release workflow similar to `spine2d`.

## Requirements

- A `CARGO_REGISTRY_TOKEN` secret in GitHub Actions (crates.io API token).
- `CHANGELOG.md` follows the Keep a Changelog format and contains a section like `## [X.Y.Z] - YYYY-MM-DD`.

## How To Release

1. Bump versions in all workspace crates to `X.Y.Z`:
   - `unity-asset-core`
   - `unity-asset-yaml`
   - `unity-asset-binary`
   - `unity-asset-decode`
   - `unity-asset` (`unity-asset-lib`)
   - `unity-asset-cli`
2. Update internal workspace dependency versions (path deps) to the same `X.Y.Z`.
3. Update `CHANGELOG.md`:
   - Move the release section from `Unreleased` to the release date.
4. Create and push a tag:
   - `git tag vX.Y.Z`
   - `git push origin vX.Y.Z`

## What The CI Does

On tag push (`vX.Y.Z`), `.github/workflows/release.yml` will:

- Validate that all crate versions match the tag (and that path-dependency versions also match).
- Run formatting, clippy, and tests.
- Publish crates to crates.io in dependency order (with retries for crates.io index propagation).
- Create a GitHub Release using notes extracted from `CHANGELOG.md`.
