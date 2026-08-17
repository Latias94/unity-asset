# Releasing

This repository releases only from an immutable `vMAJOR.MINOR.PATCH` tag through
an explicit `Release` workflow dispatch. Pushing a tag never starts publication.
The selected mode is either `dry-run` or `publish`; both check out the tag's
peeled commit, prove that its `HEAD`, package versions, dependency graph,
lockfile, and generated evidence agree, and use that commit for every subsequent
step.

## Prerequisites

- The GitHub Actions `release.yml` workflow is enabled.
- A protected GitHub Environment named `crates-io-production` has required
  reviewers, disallows self-approval and administrator bypass, and exposes the
  `CARGO_REGISTRY_TOKEN_PRODUCTION` environment secret. Delete any repository-
  scoped crates.io token; the workflow intentionally fails closed when this
  environment secret is unavailable.
- A repository ruleset protects `v*` release tags from creation, update, and
  deletion by ordinary write-capable identities. Only the release maintainers or
  release automation may create an immutable signed release tag.
- GitHub Releases are immutable after publication, or the repository limits
  Release write permission to the release automation identity. A release
  workflow proves the asset bytes it uploads, but cannot prevent a different
  identity from mutating a completed Release after the workflow exits.
- The release commit is clean and already passes the normal branch CI.
- The exact stable Rust channel tracked by `rust-toolchain.toml` is available
  for the release build. The workspace `rust-version` remains the declared MSRV
  and has its own release gate.

## Preparing a release

1. Choose the version, for example `0.4.0`.
2. Update `[workspace.package].version` and every internal requirement in root
   `Cargo.toml` to that version. Member manifests inherit the workspace values;
   they must not declare independent versions or internal path-version pairs.
3. Replace the unreleased notes with exactly one `## [<version>] - YYYY-MM-DD`
   section in `CHANGELOG.md`. The release title and body are generated from
   this tracked section and bound into release evidence.
4. Run the release eligibility checks sequentially:

   ```text
   cargo fmt --all -- --check
   cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
   cargo nextest run --workspace --locked
   cargo nextest run --workspace --all-features --locked
   cargo build --workspace --all-targets --locked
   cargo doc --workspace --all-features --no-deps --locked
   python scripts/run_real_daemon_agent.py
   python scripts/verify_workspace_packages.py --mode packages
   ```

   Package mode packages every publishable crate, unpacks the archives, and
   builds isolated consumers for default and explicitly documented feature
   profiles. Ordinary CI treats the Ubuntu `--mode full` run as the canonical
   archive authority and binary identity proof. macOS and Windows repeat package
   mode only to compile the unpacked archives and external consumers under their
   native target configuration; those runs do not define the bytes published by
   the release workflow. The release workflow repeats the full proof from the
   immutable tag before credentials are exposed.
   `--mode full` requires a clean commit because its binary source identity must
   describe the exact packaged bytes. Use `--mode preflight` for the fast metadata
   and dependency-policy check.
5. Commit the release source. Do not modify the commit after tagging it.
6. Create and push the signed tag. The tag push is deliberately inert and
   cannot publish crates or a GitHub Release:

   ```text
   git tag -s v0.4.0 -m "v0.4.0"
   git push origin v0.4.0
   ```

7. Run the `Release` workflow manually with the existing tag as its `tag` input
   and `mode` set to `dry-run`. This read-only mode executes the release tests,
   package isolation, platform matrix, cargo-dist build, exact asset assembly,
   protocol SDK generation, and final evidence plus title/body verification. It
   receives no attestation, Release-write, or crates.io publication authority.
8. After the dry-run succeeds, dispatch the `Release` workflow again with the
   same tag and `mode` set to `publish`. A signed annotated tag is mandatory.
   The source verifier rejects lightweight tags, and the workflow requires
   GitHub to report the tag signature as verified. It never trusts a branch
   name or arbitrary ref as a release source.

## What the explicit release workflow proves

Before credentials are made available, the workflow:

1. Checks out `refs/tags/<tag>` with full tag history and requires the checked
   out `HEAD` to equal the tag's peeled commit. GitHub must verify the annotated
   tag signature.
2. Writes canonical `release-evidence.json` containing the tag, tag object,
   commit, workspace version, `Cargo.lock` SHA-256, MSRV, release toolchain,
   dependency-first publish order, package manifests, the SHA-256 of the
   cargo-dist local-artifact plan, that plan's exact artifact inventory, the
   deterministic C# protocol SDK/fixture bundle identity, and the normalized
   GitHub Release title/body digests.
3. Builds every library target with Rust `1.88.0` against the locked graph.
4. Runs formatting, strict Clippy, default/async/decode/binary Rust tests, and
   the Windows, Linux, and macOS local-transport plus C# conformance matrix.
5. Packages every publishable crate and proves isolated archive consumers work
   without a repository path dependency, root patch, missing internal archive,
   or untrusted registry source.

Only after those gates pass does the workflow build `dist` binaries from the
same verified commit, check the pinned cargo-dist installer SHA-256 and
installed version, and require the produced file set to equal the precomputed
cargo-dist local plan. It creates one complete checksum inventory. In `publish`
mode a separate least-privilege job adds build provenance attestations before
the workflow creates or updates a GitHub **Draft** Release. Dry-run mode stops
after re-reading the complete bundle and the separately stored canonical title
and body proof.
Every pre-existing asset must already be byte-identical; after upload the
workflow reads the GitHub API back and verifies the exact name set and
SHA-256 of every asset.

Only after that draft has passed read-back verification does the protected
`crates-io-production` job obtain its environment secret. It rechecks the
signed tag after approval and publishes the reviewed package set in
dependency order. Every package is built and every existing remote version is
byte-verified before the first irreversible crates.io write. Only after that
complete preflight succeeds are missing crates published in dependency order.
The final job revalidates the signed tag and changes only the same verified
Draft Release to published, then reads its metadata and complete asset set back
again. If the publish PATCH succeeded but the client missed the response, a
rerun accepts only the same Release ID with byte-identical metadata and assets.

The workflow pins its release-critical actions, reads the exact Rust toolchain
from the verified tag's `rust-toolchain.toml` before any Cargo command, and pins
the cargo-dist version and installer digest. Cargo commands that resolve the
workspace use `--locked`; the cargo-dist stage additionally proves that
`Cargo.lock` did not change.

## Retrying or rebuilding a release

There is deliberately no automatic tag-push publication or arbitrary-ref asset
upload workflow. The only entry point is an explicit dispatch that checks out
`refs/tags/<tag>` and selects exactly one mode. `dry-run` has no publication
jobs or write permissions. `publish` enters the attestation, Draft Release,
protected crates.io, and final Release state machine. Never build an existing
tag from `main`, a branch, or another user-provided ref.

If a publish dispatch fails before the production approval, rerun `publish` for
the same tag. Runs for a tag and mode never cancel one another. The rerun resolves
the same peeled commit and either recreates the Draft Release or accepts only
its already byte-identical assets. If a transient
failure occurs after one or more crates are published, the publish step
downloads each existing `.crate` and requires it to be byte-identical to the
locally packaged archive before treating the operation as idempotent. A
mismatch, missing expected asset, or extra Release asset fails closed. Do not
move or recreate a release tag to repair assets; investigate the evidence and
rerun the explicit `publish` dispatch for the same tag.

## Release evidence

A completed GitHub Release contains:

- `release-evidence.json`: canonical source identity and package topology.
- `release-dist-plan.json`: the exact cargo-dist local-artifact inventory
  bound into the source evidence.
- `unity-asset-search-protocol-sdk-v<version>.zip`: the public C# reference
  codec, structural JSON schemas, and all golden protocol fixtures with an internal manifest.
- `SHA256SUMS`: checksums for every attached binary, protocol SDK, and
  provenance file.

The release is also accompanied by GitHub build provenance attestations for the
binaries, checksums, and source evidence. Those attestations live in GitHub's
attestation service rather than as ordinary Release attachments.

These files establish which tag, commit, lockfile, toolchain, and crate graph
produced the release. Provenance and checksums complement source verification;
they do not permit a different ref to stand in for the tag commit.

## Unity Editor plugin packaging (scheme B)

The Unity Editor UPM plugin vendors the daemon binaries into:

- `Packages/<package-id>/Tools/<platform>/`

The Unity plugin release workflow should:

1. Download `unity-asset-search-daemon` archives from this repository's GitHub
   Release.
2. Extract and place them into `Tools/win-x64/`, `Tools/linux-x64/`, and
   `Tools/mac-universal/`.
3. For macOS, merge `x86_64` and `aarch64` binaries into a universal binary,
   for example with `lipo -create`.
4. Ensure macOS and Linux binaries are executable with `chmod +x`.
