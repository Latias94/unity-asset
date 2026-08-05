# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Added `AssetWorkspace` as the authoritative owner of immutable source catalogs, content-addressed backing bytes, workspace revisions, snapshots, and prepared views.
- Added the versioned `WorkspaceInspector` source and object projections, exact structured lookup, streamed-resource resolution, and the allocation-free `workspace_capabilities` catalog.
- Added canonical `MutationPlan` v2 JSON/YAML contracts with workspace, revision, source fingerprint, object digest, operation-order, and semantic guard validation.
- Added zero-write prepare with independently reparsed segmented artifacts, read-your-writes inspection, deterministic proof manifests, and an opaque single-use `PreparedChange`.
- Added recoverable in-place publication with validated containment roots, durable journals, directory identity binding, `CommitReport`, recovery discovery, `RecoveryLocator`, `RecoveryOutcome`, and `RollbackReceipt` contracts.
- Added one revision-bound `ReferenceGraph` with typed field paths, coverage, resolution states, incoming/outgoing/closure queries, caching, and deterministic JSON, JSON Lines, and DOT projections.
- Added typed extraction request v2, plan v4, manifest v3, and report v3 contracts with bundle-container and reference-traversal queries, deterministic artifact paths, caller-budgeted execution, recoverable publication, and verified resume.
- Added a revision-bound, consumer-owned search pipeline with transaction-keyed `ChangeSet` handoff, coherent `SearchGeneration` state, project-bound local IPC Bootstrap V2, and bounded idempotent reindex operations.
- Added deterministic tag-release evidence, exact cargo-dist artifact-plan validation, isolated archive-consumer verification, real MSRV validation, pinned release inputs, checksums, GitHub Draft Release byte read-back, and build provenance attestations.
- Added schema-aware mutation recipes that inspect exact YAML or binary provenance and lower guarded field, reference, schema, sequence, hierarchy, UnityEvent, material, transform, and streamed-audio changes into plan fragments.
- Added deterministic performance contracts for unified TypeTree traversal and segmented prepared artifacts.

### Changed

- **Breaking:** Replaced mutable high-level loading and editing with `AssetWorkspace`, immutable `WorkspaceView` values, guarded plans, prepare, preview, recoverable commit, and explicit recovery.
- **Breaking:** Replaced broad text-oriented CLI inspection and export surfaces with typed `workspace`, `references`, `export`, and `split-yaml` commands that exchange versioned JSON contracts.
- **Breaking:** Replaced the 0.3 search HTTP `/v1` and unreleased `/v2`/`/v3` development transports with project-bound local IPC, Bootstrap V2, and business revision 2; removed bearer-token configuration, `IndexProgress`, and compatibility fallback.
- **Breaking:** Unified TypeTree read, skip, PPtr scan, validation, encoding, and byte-preserving rewrite on one compiled `TypeTreeSchema`; TypeTree write errors now use `TypeTreeWriteError`, and `unity_asset_write::Endian` was replaced by the shared `ByteOrder`.
- Made JSON and TPK TypeTree ingestion caller-budgeted, deterministic, depth-bounded, and immutable; workspace loads retain only required schemas in frozen per-source registries.
- Reworked source loading, archive/WebFile/bundle expansion, edits, serialization, and publication to stage complete candidate state before one authority-changing commit.
- Replaced contiguous output images with budgeted seekable segment graphs that retain unchanged source ranges, independently reparse candidate artifacts, and stream verified bytes during publication.
- Bound source fingerprints, object identities, prepared artifacts, journals, extraction manifests, changes, and search generations to the versioned BLAKE3 `DigestV1` contract.
- Made binary reference changes allocate external-file IDs deterministically and preserve exact PPtr shape, directory occurrence order, and source ownership.
- Moved optional decode representations behind extraction policies while keeping authoritative selection, identity, planning, and manifest semantics in the high-level workspace.
- Bound extraction final-path evidence reads to one cumulative, recovery-adjustable verification limit.

### Fixed

- Hardened parsing, decompression, registry loading, recursive serialization, semantic cloning, and artifact construction so caller-owned budgets are charged before allocation or expansion.
- Preserved wire-faithful SerializedFile version gates, signed path IDs, byte order, alignment, type tables, external tables, object proof ranges, and unchanged object bytes across rebuilds.
- Preserved AssetBundle signature, directory-node semantics, duplicate occurrences, compression policy, and exact encoded block ranges; unsupported encrypted layouts now fail structurally.
- Made publication restart-safe across interruption, partial replacement, stale evidence, destination drift, directory replacement, and idempotent recovery redelivery.
- Made search reindex admission idempotent by transaction identity and resilient to queueing, retries, worker failure, cancellation, and conflicting payloads.

### Removed

- **Breaking:** Removed the superseded mutable aggregate loader, direct edit/save lifecycle, pending write state, and implicit filesystem dependency probing.
- **Breaking:** Removed mutable serialized-file edit sessions, change trackers, bare-byte object replacement, and raw object mutation escape hatches.
- **Breaking:** Removed duplicate TypeTree builders, serializers, traversal facades, registries, semantic digest engines, and reference models.
- **Breaking:** Removed obsolete CLI commands whose behavior is now represented by typed workspace inspection, reference projection, extraction, or low-level format examples.
- **Breaking:** Removed placeholder Mesh decoding/export, fabricated binary metadata summaries, cached Bundle loader facades, and implicit Unity version defaults rather than reporting capabilities or observations the library cannot prove.
- Removed production dependence on callback-based script-schema generation; immutable JSON/TPK registries are now the workspace boundary.

## [0.3.0] - 2026-01-27

### Highlights

- Added UnityPy-style AssetBundle container discovery with glob patterns and case-insensitive matching.
- Added best-effort cross-file reference analysis for SerializedFiles and a reusable local search stack.
- Introduced the search core, index, daemon, and CLI crates for downstream tools.

### Added

- Added bundle-container path discovery and optional indexing as `BundleContainer` search documents.
- Added superseded aggregate diagnostics for bundle headers, SerializedFile metadata, and signed path-ID distributions; current inspection uses `WorkspaceInspector`.
- Added TypeTree PPtr extraction, external GUID resolution, and early incremental reference-analysis helpers.
- Added a superseded class-helper layer for common YAML UI controls and binary GameObject, Transform, RectTransform, SpriteRenderer, Sprite, and SpriteAtlas changes.
- Added legacy UnityWeb and UnityRaw repacking for supported versions.
- Added directory-wide `.meta` GUID indexing and best-effort Unity project discovery.
- Added JSON/TPK TypeTree registries and the optional external script-schema exporter documented in `docs/SCRIPT_TYPETREES.md`.
- Added search flags for bundle-container indexing and ignore policy, progress reporting, asynchronous reindex orchestration, and an experimental Unity Editor client.
- Added `cargo-dist` release automation and a manual workflow for repairing missing release assets.

### Changed

- Updated bundle name and type filters to inspect embedded asset names and actual object presence.

### Fixed

- Populated bundle compression metadata and object reference summaries.
- Prevented YAML serialization from emitting placeholder mappings for complex block-array elements.
- Accepted UnityCN/Tuanjie version suffixes and ignored parenthesized revisions for comparison.
- Preserved rare unnamed TypeTree children, normalized PPtr input shapes, and supported the legacy version-2 variable-count field.
- Added metadata-at-end parsing and saving for pre-version-9 SerializedFiles.
- Corrected legacy UnityWeb and UnityRaw header and directory-offset handling.
- Improved external reference resolution through normalized physical paths and retained null bundle-container pointers as unresolved occurrences.

### Breaking Changes

- None intended. In the 0.x series, breaking changes may occur between minor versions.

## [0.2.0] - 2025-12-26

### Highlights

- Split the project into parsing, high-level loading, optional decode, and CLI crates.
- Added on-demand binary object handles, fast name peeking, reference scanning, and early discovery/export workflows.
- Made TypeTree policy explicit and replaced library stderr logging with structured warnings.
- Expanded UnityFS, WebFile, streamed-resource, and stripped-TypeTree coverage.

### Breaking Changes

- Split user-facing, CLI, decode, core, YAML, and binary concerns into separate crates.
- Made decode/export opt-in through feature flags or the dedicated decode crate.

### Added

- Added a superseded high-level YAML/binary loader for cross-container iteration.
- Added `ObjectHandle` for on-demand reads and fast `peek_name`.
- Added composable JSON/TPK TypeTree registries for stripped assets.
- Added early CLI inspection, reference scanning, and manifest-based export workflows.

### Changed

- Made strict versus lenient TypeTree parsing caller-controlled and routed warnings through structured collectors.
- Optimized repeated binary object lookup with a lazy path-ID index.

### Fixed

- Corrected UnityFS archive flags, version-sensitive SerializedFile headers and object tables, WebFile detection and decompression, TypeTree alignment, and common-string lookup.

### Security

- Added bounded string reads, checked arithmetic, decompression ceilings, and parser resource limits.

## [0.1.0] - 2025-08-27

### Added

- Added Unity YAML multi-document parsing.
- Added AssetBundle and SerializedFile parsing with LZ4, LZMA, Brotli, and Gzip support.
- Added optional async parsing APIs and synchronous/asynchronous CLI tools.
- Added early AudioClip, Texture2D, Sprite, Mesh, and TypeTree processing.
- Added recursive batch processing, JSON/YAML/debug output, progress reporting, and configurable concurrency.

### Architecture

- Added `unity-asset-core`, `unity-asset-yaml`, `unity-asset-binary`, `unity-asset`, and `unity-asset-cli`.

### Known Limitations

- Texture decoding covered a limited set of basic uncompressed formats.
- Some Unity 5.x LZMA variants remained unsupported.

### Acknowledgments

- [UnityPy](https://github.com/K0lb3/UnityPy) by @K0lb3
- [unity-rs](https://github.com/yuanyan3060/unity-rs) by @yuanyan3060
