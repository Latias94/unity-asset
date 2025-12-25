# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- See [0.2.0] (Unreleased)

### Changed
- See [0.2.0] (Unreleased)

### Fixed
- See [0.2.0] (Unreleased)

## [0.2.0] - Unreleased

### Added
- `docs/REFACTORING.md`: a UnityPy-aligned fearless refactor roadmap (layering, strict/lenient parsing, decode split, API discipline).
- `unity-asset-decode`: a new crate that hosts decode/export helpers (Texture/Audio/Sprite/Mesh) on top of `unity-asset-binary`.
- `BinarySource: Display` to provide a single, consistent string representation across library diagnostics and CLI output.
- `unity-asset` `EnvironmentOptions` + `EnvironmentWarning` to control parsing behavior and observe non-fatal load issues without printing from libraries.
- `unity-asset` CLI: `--strict` (fail-fast TypeTree parsing) and `--show-warnings` (print collected warnings and TypeTree warnings in `inspect-object`).
- `unity-asset-yaml` loader: `load_yaml_with_warnings` / `load_yaml_async_with_warnings` to surface non-fatal per-document conversion failures without printing from library code.
- UnityPy-style `ObjectHandle` in `unity-asset-binary` to treat objects as lightweight, on-demand readers (`SerializedFile::object_handles` / `SerializedFile::find_object_handle`).
- `unity-asset-binary` `ObjectHandle::peek_name()` to read `m_Name`/`name` via a TypeTree prefix fast path (without parsing the full object).
- `unity-asset-binary` external TypeTree registry API (`TypeTreeRegistry`, `JsonTypeTreeRegistry`) for best-effort parsing of stripped assets.
- `unity-asset-binary::file` unified loader (`load_unity_file` / `load_unity_file_from_memory`) and a layered `unity-asset-binary::formats::*` namespace.
- `unity-asset` `Environment` can now load WebFiles and treat contained bundles/assets as first-class binary sources (including streamed resource reads from WebFile entries).
- Optional object data preloading toggle in `SerializedFileParser` to enable future lazy-loading workflows.
- UnityPy-like `Environment` API in the `unity-asset` crate to load YAML + binary files and iterate objects.
- `Environment` helpers to find YAML objects by anchor and binary objects by `path_id`.
- `Environment` helpers to find binary objects within a specific source (bundle/asset path) and within a bundle asset index.
- `BinaryObjectKey` + `Environment::read_binary_object_key` to provide globally-unique, round-trippable binary object references across bundles/serialized files.
- Best-effort Unity `PPtr` (`fileID`/`pathID`) resolution helpers via `Environment::resolve_binary_pptr` / `Environment::read_binary_pptr`.
- Best-effort AssetBundle `m_Container` extraction and lookup helpers to find objects by asset path.
  - Includes a raw parsing fallback for stripped TypeTree bundles (experimental, version-dependent).
  - Includes a last-resort in-bundle lookup by `path_id` when external `fileID` mapping cannot be resolved.
- `unity-asset` CLI: `export-bundle` command to export bundle container-matched objects as raw `.bin`.
- `unity-asset` CLI: `export-bundle --decode` to export `AudioClip` (prefer encoded/embedded bytes; `.wav` fallback) and `Texture2D` as `.png` (best-effort; falls back to raw `.bin`).
- `unity-asset` CLI: `export-bundle --jobs` to parallelize export/decode work (0 = auto, 1 = serial).
- `unity-asset` CLI: `export-bundle --class-id/--class-name` filtering, and `--overwrite/--skip-existing` output behavior controls.
- `unity-asset` CLI: `export-bundle --manifest <path>` to write a JSON export manifest for resume/regression checks.
- `unity-asset` CLI: `export-bundle --resume <manifest>` to skip already-exported entries when re-running.
- `unity-asset` CLI: `export-bundle --continue-on-error` to record failures into the manifest and continue exporting.
- `unity-asset` CLI: `export-bundle --retry-failed-from <manifest>` to re-export only previously failed entries.
- `unity-asset` CLI: `list-bundle` command to list bundle nodes (files) for debugging/inspection (uses fast bundle parsing; does not preload assets/decompress blocks).
- `unity-asset` CLI: `find-object` command to search AssetBundle `m_Container` entries and print resolvable object keys (uses fast bundle parsing; avoids preloading bundle assets).
- `unity-asset` CLI: `find-object --name` to filter by object `m_Name`/`name` via a TypeTree prefix fast path (best-effort).
- `unity-asset` CLI: `--typetree-registry <path>` to load an external TypeTree registry for stripped assets (best-effort).
- `unity-asset` CLI: `dump-typetree-registry` to generate a JSON registry from loaded files.
- `unity-asset` CLI: `--typetree-registry` supports UnityPy-compatible `.tpk` TypeTree packs.
- `unity-asset` CLI: `scan-pptr` to scan `PPtr` references (`fileID`/`pathID`) without fully parsing objects (uses fast bundle parsing when possible; falls back to `Environment` otherwise).
- `unity-asset` CLI: `deps` to build a best-effort dependency graph (summary/edges/dot/json) via TypeTree PPtr scanning.
- `unity-asset` CLI: `inspect-object` command to inspect a single binary object by (source, asset_index, path_id) and print a TypeTree-derived field tree for debugging.
- `unity-asset` CLI: `find-object` supports `--class-id` / `--class-name` filtering for easier batch workflows.
- `Environment::read_stream_data_from_fs` to load streamed `.resS`/`.resource` payloads from the filesystem when they are not embedded in a bundle.
- Golden regression tests for core workflows (`tests/golden/golden_v1.json` + `unity-asset` `golden_regression_smoke`).

### Changed
- Improved UnityPy parity for `SerializedFile` parsing (object table, script types, file identifiers, and version-dependent fields).
- (BREAKING) Unified binary object model: `UnityObject` now wraps `asset::ObjectInfo` + parsed `UnityClass` instead of maintaining a duplicated `ObjectInfo`.
- (BREAKING) `SerializedFileParser::from_bytes` now defaults to lazy object data access to avoid copying per-object buffers (use `from_bytes_with_options(data, true)` to restore eager preloading).
- (BREAKING) `SerializedFileHeader` now stores v22+ `file_size` / `data_offset` as `u64` (no truncation), and rejects negative header values.
- (BREAKING) `unity-asset-binary` no longer mass re-exports types/functions at the crate root; import from `bundle` / `asset` / `object` / `typetree` (or `formats::*`) instead.
- (BREAKING) `BinaryObjectKey` now defaults to a `bok2|...` string format to support WebFile entry sources; `bok1|...` is still accepted for parsing.
- Metadata dependency analysis now scans TypeTree values for PPtr references (`fileID`/`pathID`) to build an object dependency graph.
- Metadata relationship analysis now builds a best-effort GameObject/Transform hierarchy and GameObject->Component mapping (TypeTree-based).
- Component relationships now include best-effort per-component dependency lists (derived from the dependency graph).
- External references now include best-effort resolved `file_path`/`guid`, and component relationships also expose external dependencies.
- `AssetReference` is now populated from both internal and external dependency graphs for quick “what references what” reporting.
- Clarified project maturity in documentation (binary/metadata/hierarchy analysis is still WIP).
- Ignored `repo-ref/` in `.gitignore` to avoid accidentally committing reference sources.
- For large objects without TypeTree, `_raw_data` is no longer expanded into a full byte array; use `UnityObject::raw_data()` (properties now include `_raw_data_len` and a small `_raw_data_preview`).
- `Environment` now caches best-effort AssetBundle `m_Container` extraction results to avoid repeated parsing during lookups.
- `AssetBundle` now tracks parsed asset file names (`asset_names`) to help resolve in-bundle references.
- `unity-asset-binary` `SerializedFileParser::from_shared_range*` to parse embedded/packed SerializedFiles from a shared backing buffer without copying bytes (best-effort).
- `unity-asset-binary` `AssetBundle::{extract_file_slice, extract_node_slice}` to access bundle entry bytes without allocating.
- `unity-asset-binary` `BundleLoadOptions::lazy()` to validate bundle metadata without preloading assets or decompressing blocks.
- `unity-asset-binary` `BundleParser::from_shared_range*` to parse AssetBundles from a shared backing buffer + byte range (enables true zero-copy WebFile/mmap bundle loading).
- UnityFS bundles loaded with `BundleLoadOptions::fast()` now record the original compressed bytes and decompress blocks on first access (`AssetBundle::data_checked` / `data_arc` / `extract_*`).
- (BREAKING) `BundleParser::from_slice*` now copies bytes; use `from_shared_range*` for zero-copy parsing.
- `unity-asset-binary` `file::load_unity_file_from_shared_range` to parse Unity files from a shared backing buffer + byte range (enables zero-copy WebFile entry loading).
- `unity-asset-binary` WebFile `from_shared_range` + `extract_file_view`/`extract_file_slice` for zero-copy WebFile entry access (best-effort).
- `unity-asset`/`unity-asset-cli` enable the `mmap` feature by default to reduce peak memory usage when loading from filesystem paths.
- Marked the most comprehensive UnityPy-port integration tests as `#[ignore]` by default to keep `cargo test` fast (see `CONTRIBUTING.md` for running ignored tests).
- Reduced duplicated bundle parsing in `Environment` unit tests to speed up the default `cargo test` loop.
- `find-object --verbose` now prints a copy/paste-able `BinaryObjectKey` string which can be fed into `inspect-object --key`.
- (BREAKING) `BinaryObjectRef` / `EnvironmentObjectRef` are no longer `Copy` to support reporter/warning plumbing.
- `unity-asset` CLI warning output is now centralized via `EnvironmentReporter` (no more per-command manual draining/printing).
- (BREAKING) `EnvironmentReporter` is now `Send + Sync` and stored behind `Arc`, making `Environment` `Send + Sync` for concurrency-ready workflows.
- (BREAKING) `unity-asset-binary` is now parser-only; decode/export helpers moved to the new `unity-asset-decode` crate (enable `unity-asset-decode/full` or specific features like `texture`/`audio`).
- `unity-asset-cli` now has an explicit `decode` feature (enabled by default) to allow building a lighter CLI with `--no-default-features`.
- TypeTree array parsing now uses endianness-aware fast paths for common numeric primitive arrays (`SInt16/UInt16/SInt32/UInt32/SInt64/UInt64/float/double`).
- (BREAKING) `UnityValue` now includes `Bytes(Vec<u8>)` and TypeTree parsing emits `Bytes` for `TypelessData` and byte arrays (`UInt8`/`char`/`SInt8`), reducing allocations for large objects.
- (BREAKING) `BundleLoadOptions` now includes explicit resource limits (`max_blocks_info_size`, `max_blocks`, `max_nodes`) and bundle parsing enforces these limits.
- (BREAKING) Removed the unimplemented `unity-asset-binary` `xz2` feature to avoid implying improved Unity LZMA compatibility.
- (BREAKING) `SerializedFile` can now be a zero-copy view into a shared backing buffer (e.g. bundle-decompressed data); `SerializedFile::data_arc()` returns the backing buffer and `SerializedFile::data()` returns the file view.
- Dependency analysis now scans TypeTree streams for `PPtr` references without allocating full parsed objects, improving performance on large assets.
- Best-effort TypeTree support for managed references: parses `ReferencedObjectData` payloads via `SerializedFile.ref_types` (Unity 2019+) and skips `ManagedReferencesRegistry` nodes to keep parsing fast.
- When managed reference payload types cannot be resolved, `ReferencedObject` now includes `_referenced_type_unresolved=true` and `_referenced_type_key=\"class|ns|asm\"` for explainable fallbacks.

### Fixed
- Hardened length-prefixed string reads to avoid hostile allocations and out-of-bounds reads (length is validated against remaining bytes and a maximum limit).
- Hardened UnityFS/legacy bundle parsing against hostile metadata (rejects negative counts/offsets, enforces `max_memory`/metadata caps before allocation/decompression).
- Hardened UnityFS/legacy bundle parsing against hostile *compressed-size* metadata (caps compressed blocks info reads and legacy directory reads before allocating).
- Reduced peak memory usage when loading assets from UnityFS bundles by avoiding both an extra full-buffer clone and per-asset file byte copies (best-effort).
- Hardened WebFile parsing against hostile metadata (rejects negative header sizes/offsets/lengths and enforces basic bounds checks).
- Prevented integer overflow in LZ4 buffer sizing for large `uncompressed_size` values.
- `Environment::load_file` now attempts binary detection for extension-less files (best-effort), improving support for `UnityWebData*` and other build artifacts.
- Fixed `UnityFile` sniffing to avoid mis-classifying uncompressed `UnityWebData*` WebFiles as legacy `UnityWeb` bundles.
- Removed `println!`/`eprintln!` from library code paths (warnings are returned/collected instead of writing to stderr).
- YAML loader no longer prints per-document conversion failures; these are surfaced as warnings instead.
- Fixed v<9 endian seek underflow in `SerializedFileHeader` parsing (checked arithmetic + explicit error).
- Fixed UnityFS archive flags handling to honor `BlocksInfoAtEnd` / padding behavior (and corrected flag constants to match UnityPy).
- Fixed UnityWeb decompression to prefer the header’s explicit `uncompressed_size` (guessing is only a fallback).
- TypeTree parsing no longer writes to stderr from library code; added strict/lenient parsing options with structured warnings.
- `SerializedFile::find_object` now uses a lazy `path_id` index for near O(1) lookups after first query.
- Correct handling of `big_id_enabled` and other version-sensitive header/object fields.
- `AudioClip` raw parsing now correctly extracts `StreamedResource` (`m_Source`/`m_Offset`/`m_Size`) and avoids treating the resource path bytes as embedded audio data.
- `Texture2D` TypeTree parsing now recognizes `m_StreamData` and enables streamed texture bytes to be loaded via the same resource-reading path as AudioClip.
- `export-bundle --decode` now exports `TextAsset` payloads (best-effort) by preferring `m_Script`/`m_Text` and falling back to `m_Bytes`.
- `export-bundle --decode` now exports `Sprite` images (best-effort) by resolving the referenced `Texture2D` and cropping the sprite rect.
- TypeTree blob parsing now resolves high-bit (0x80000000) string offsets via Unity common strings, reducing `Null`/missing-field issues when parsing objects with stripped/compact TypeTrees.

## [0.1.0] - TBD (First Release)

### Added

#### Core Features
- **YAML Processing**: Unity YAML format support with multi-document parsing
- **Binary Asset Processing**: AssetBundle and SerializedFile parsing with compression support
- **Type Safety**: Rust's type system prevents common parsing vulnerabilities
- **Async/Await API**: Optional async support for all parsing operations
- **CLI Tools**: Both synchronous and asynchronous command-line interfaces

#### Supported Formats
- **YAML Files**: .asset, .prefab, .unity, .meta files
- **Binary Assets**: AssetBundle (UnityFS, UnityWeb, UnityRaw), SerializedFile
- **Compression**: LZ4, LZMA, Brotli, Gzip support
- **Unity Versions**: 3.4 - 2023.x compatibility

#### Object Processing
- **AudioClip**: Audio processing with sample extraction
- **Texture2D**: Basic texture processing (RGBA32, RGB24, ARGB32, Alpha8)
- **Sprite**: Sprite parsing and metadata extraction
- **Mesh**: Mesh data structure parsing
- **TypeTree**: TypeTree parsing and manipulation

#### CLI Features
- **Batch Processing**: Recursive directory scanning and processing
- **Multiple Output Formats**: JSON, YAML, debug formats
- **Progress Reporting**: Real-time progress bars and statistics
- **Configurable Concurrency**: Adjustable parallel processing

### Architecture
- `unity-asset-core`: Core data structures and traits
- `unity-asset-yaml`: YAML format parsing and serialization
- `unity-asset-binary`: Binary asset parsing (AssetBundle, SerializedFile)
- `unity-asset-lib`: Main library crate
- `unity-asset-cli`: Command-line tools

### Known Limitations
- **Texture Formats**: Limited to basic uncompressed formats (RGBA32, RGB24, ARGB32, Alpha8)
- **LZMA Decompression**: Some Unity 5.x files with specific LZMA variants may fail to decompress

### Acknowledgments
This project builds upon:
- [UnityPy](https://github.com/K0lb3/UnityPy) by @K0lb3
- [unity-rs](https://github.com/yuanyan3060/unity-rs) by @yuanyan3060
