# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2025-12-26

### Highlights
- Publish the search stack to crates.io:
  - `unity-asset-search-core` and `unity-asset-search-index` (reusable library crates)
  - `unity-asset-search-daemon` and `unity-asset-search-cli` (tools)
- Multi-platform release assets for UnityHero packaging (scheme B):
  - `unity-asset-search-daemon` (embedded into UnityHero `Tools/<platform>/`)
  - `unity-asset-search-cli` (optional; debugging/ops utility)
- Release automation upgrades via `cargo-dist` to ship binaries alongside the GitHub Release.

### Added
- `cargo-dist` configuration and CI to build and attach platform archives to GitHub Releases.
- A manual GitHub Actions workflow to backfill missing dist assets for an existing tag (repair path).

### Breaking Changes
- None intended. As a reminder, in the 0.x series breaking changes may occur between minor versions.

## [0.2.0] - 2025-12-26

### Highlights
- Major refactor and crate split to support a clear layered architecture (parsing → handles → environment → decode).
- UnityPy-like discovery and export workflows:
  - fast object handles (`ObjectHandle`) and `peek_name` for large scans
  - `find-object`, `scan-pptr`, `deps`, `export-bundle` in the CLI
- Optional decode/export helpers moved into `unity-asset-decode` (Texture2D/Sprite/AudioClip/Mesh), kept out of the core parser by default.
- Safer and more predictable parsing (strict vs lenient TypeTree modes, structured warnings, no library stderr logging).
- Better coverage of real-world Unity layouts (UnityFS/WebFile detection, streamed resources, stripped TypeTree fallbacks via external registries).

### Breaking Changes
- This is a large refactor release. In the 0.x series, breaking API changes may occur between minor versions.
- Crates are now split by concern:
  - `unity-asset` (user-facing library), `unity-asset-cli` (CLI), `unity-asset-decode` (optional decode/export),
    plus `unity-asset-core`, `unity-asset-yaml`, `unity-asset-binary`.
- Decode/export is opt-in (CLI `--features decode`, or use `unity-asset-decode` directly).

### Added
- High-level `Environment` API in `unity-asset` for loading YAML + binary sources and iterating objects across AssetBundles, SerializedFiles, and WebFiles.
- `ObjectHandle` for on-demand object reads (UnityPy-style “ObjectReader”-like handle) and fast `peek_name`.
- External TypeTree registry support (JSON/TPK; composable registries) for best-effort parsing when TypeTree is stripped.
- User-facing CLI workflows:
  - inspection/discovery: `list-bundle`, `list-objects`, `find-object`, `inspect-object`
  - scanning/graphs: `scan-pptr`, `deps`
  - export: `export-bundle` (manifest/resume; optional `--decode`)
- Documentation: `docs/REFACTORING.md` describes the refactor plan, constraints, and future work.

### Changed
- TypeTree parsing is user-controlled (strict vs lenient) and reports warnings via structured collectors instead of printing.
- Bulk object lookup is optimized for repeated queries (lazy `path_id` index; avoids unnecessary copies).

### Fixed
- UnityFS archive flags handling (including `BlocksInfoAtEnd`) and several version-sensitive SerializedFile header/object edge cases.
- WebFile detection and decompression behavior, including correct handling of uncompressed `UnityWebData*` containers.
- TypeTree alignment and common-string resolution edge cases that previously caused misreads or missing fields.

### Security
- Hardened parsing against hostile inputs (bounded string reads, checked arithmetic, decompression/resource limits).

## [0.1.0] - 2025-08-27

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
- `unity-asset`: Main library crate
- `unity-asset-cli`: Command-line tools

### Known Limitations
- **Texture Formats**: Limited to basic uncompressed formats (RGBA32, RGB24, ARGB32, Alpha8)
- **LZMA Decompression**: Some Unity 5.x files with specific LZMA variants may fail to decompress

### Acknowledgments
This project builds upon:
- [UnityPy](https://github.com/K0lb3/UnityPy) by @K0lb3
- [unity-rs](https://github.com/yuanyan3060/unity-rs) by @yuanyan3060
