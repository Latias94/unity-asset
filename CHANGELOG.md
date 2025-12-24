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
- UnityPy-style `ObjectHandle` in `unity-asset-binary` to treat objects as lightweight, on-demand readers (`SerializedFile::object_handles` / `SerializedFile::find_object_handle`).
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
- `unity-asset` CLI: `list-bundle` command to list bundle nodes (files) for debugging/inspection.
- `unity-asset` CLI: `find-object` command to search AssetBundle `m_Container` entries and print resolvable object keys.
- `unity-asset` CLI: `inspect-object` command to inspect a single binary object by (source, asset_index, path_id) and print a TypeTree-derived field tree for debugging.
- `unity-asset` CLI: `find-object` supports `--class-id` / `--class-name` filtering for easier batch workflows.
- `Environment::read_stream_data_from_fs` to load streamed `.resS`/`.resource` payloads from the filesystem when they are not embedded in a bundle.

### Changed
- Improved UnityPy parity for `SerializedFile` parsing (object table, script types, file identifiers, and version-dependent fields).
- (BREAKING) Unified binary object model: `UnityObject` now wraps `asset::ObjectInfo` + parsed `UnityClass` instead of maintaining a duplicated `ObjectInfo`.
- (BREAKING) `SerializedFileParser::from_bytes` now defaults to lazy object data access to avoid copying per-object buffers (use `from_bytes_with_options(data, true)` to restore eager preloading).
- (BREAKING) `SerializedFileHeader` now stores v22+ `file_size` / `data_offset` as `u64` (no truncation), and rejects negative header values.
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
- Marked the most comprehensive UnityPy-port integration tests as `#[ignore]` by default to keep `cargo test` fast (see `CONTRIBUTING.md` for running ignored tests).
- `find-object --verbose` now prints a copy/paste-able `BinaryObjectKey` string which can be fed into `inspect-object --key`.

### Fixed
- Hardened length-prefixed string reads to avoid hostile allocations and out-of-bounds reads (length is validated against remaining bytes and a maximum limit).
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
