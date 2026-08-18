# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-08-18

### Highlights

- Rebuilt high-level asset work around `AssetWorkspace`: callers now inspect immutable revisions, author guarded mutation plans, preview prepared output, and recover interrupted publication without reconstructing internal proof or ordering rules.
- Unified YAML and binary references, schema-aware mutations, streamed-resource extraction, and local search around revision-bound identities and caller-owned budgets, so automation receives typed evidence instead of best-effort summaries.
- Added a project-bound local search service over capability-authenticated loopback HTTP, with one canonical JSON contract shared by Rust, the CLI, C#, and future MCP adapters; 0.4.0 publishes that business protocol for the first time as revision 1.
- Deepened UnityFS, WebFile, UnityWeb, UnityRaw, SerializedFile, TypeTree, PPtr, AudioClip, Texture2D, and Sprite handling while preserving source bytes, format semantics, and bounded resource use.
- Hardened package and binary releases with isolated archive consumers, signed-source evidence, deterministic SDK artifacts, checksums, remote byte verification, and provenance attestations.

### Added

- Published business search protocol revision 1 with bounded background-reindex discovery, structured process-lifetime task failure evidence, explicit control rejection for internal operations, Draft 2020-12 structural schemas, and a deterministic SDK bundle containing the schema and fixture contract.

- Added `AssetWorkspace` as the authoritative owner of immutable source catalogs, content-addressed backing bytes, workspace revisions, snapshots, and prepared views.
- Added the versioned `WorkspaceInspector` source and object projections, exact structured lookup, streamed-resource resolution, and the allocation-free `workspace_capabilities` catalog.
- Added canonical `MutationPlan` v3 JSON/YAML contracts with workspace, revision, source fingerprint, object digest, operation-order, and semantic guard validation.
- Added zero-write prepare with independently reparsed segmented artifacts, read-your-writes inspection, deterministic proof manifests, and an opaque single-use `PreparedChange`.
- Added recoverable in-place publication with validated containment roots, durable journals, directory identity binding, `CommitReport`, recovery discovery, `RecoveryLocator`, `RecoveryOutcome`, and `RollbackReceipt` contracts; retries remain safe across interruption, partial replacement, stale evidence, and destination drift.
- Added one revision-bound `ReferenceGraph` with typed field paths, coverage, resolution states, incoming/outgoing/closure queries, caching, and deterministic JSON, JSON Lines, and DOT projections.
- Added typed extraction request v4, plan v8, manifest v6, and report v6 contracts with bundle-container and reference-traversal queries, deterministic artifact paths, caller-budgeted execution, recoverable publication, verified resume, and explicit representation-semantics identities.
- Added a YAML-only extraction request profile and identity-first `file-id-*`/`ordinal-*` artifact paths for agent-safe document export.
- Added a revision-bound, consumer-owned search pipeline with transaction-keyed `ChangeSet` handoff, coherent `SearchGeneration` state, capability-authenticated loopback HTTP discovery, and bounded idempotent reindex operations resilient to retries, worker failure, cancellation, and conflicting payloads.
- Added deterministic tag-release evidence, exact cargo-dist artifact-plan validation, isolated archive-consumer verification, real MSRV validation, pinned release inputs, checksums, GitHub Draft Release byte read-back, and build provenance attestations.
- Added schema-aware mutation recipes that inspect exact YAML or binary provenance and lower guarded field, reference, schema, sequence, hierarchy, UnityEvent, material, transform, and streamed-audio changes into plan fragments.
- Added a budgeted, immutable AssetRipper TypeTreeDump registry with release/editor mode selection for stripped assets. ([#2](https://github.com/Latias94/unity-asset/pull/2); thanks [@JomerDev](https://github.com/JomerDev))

### Changed

- **Breaking:** Replaced the mutable `Environment` aggregate loader, direct edit/save lifecycle, pending write state, and implicit filesystem probing with `AssetWorkspace`, immutable `WorkspaceView` values, guarded plans, prepare, preview, recoverable commit, and explicit recovery.
- **Breaking:** Replaced broad text-oriented CLI inspection and export surfaces with typed `workspace`, `references`, `export`, and `split-yaml` commands that exchange versioned JSON contracts; `split-yaml` now emits the canonical extraction report and manifest instead of a parallel YAML-split report.
- **Breaking:** Replaced string YAML anchors in object identities with canonical non-zero numeric `YamlFileId` values, rejected ambiguous spellings such as `01`, and changed compact object addresses from `oa1:` to `oa2:`.
- **Breaking:** Removed the legacy media Processor/Converter APIs, owned AudioClip/Texture2D/Sprite carriers, context-free quick decoders, generic image/audio exporters, and public swizzler utilities. Strict `AudioClipLayout`, `Texture2DLayout`, `SpriteLayout`, and `Prepared*` artifacts are now the only media preparation path.
- **Breaking:** Removed MP3 and AAC from strict prepared-audio descriptors until a bounded full-codec validator exists; decoded-preferred extraction now falls back to raw bytes and decoded-required extraction reports typed unsupported encoding.
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

### Removed

- **Breaking:** Removed mutable serialized-file edit sessions, change trackers, bare-byte object replacement, and raw object mutation escape hatches.
- **Breaking:** Removed placeholder Mesh decoding/export, fabricated binary metadata summaries, cached Bundle loader facades, and implicit Unity version defaults rather than reporting capabilities or observations the library cannot prove.

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
