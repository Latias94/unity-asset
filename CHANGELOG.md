# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - Not Released

### Highlights
- Better “UnityPy-style” discovery and export workflows:
  - AssetBundle `m_Container` discovery supports glob patterns (`*`, `?`) and case-insensitive matching.
  - Environment-wide dependency graph for SerializedFiles (TypeTree + PPtr scan), with best-effort external resolution.
- Search stack is ready for downstream use:
  - `unity-asset-search-core` / `unity-asset-search-index` (library crates)
  - `unity-asset-search-daemon` / `unity-asset-search-cli` (tools)

### Added
- Glob matching for AssetBundle `m_Container` discovery (`*`, `?`) and CLI support for glob patterns.
- `unity-asset-cli stats` to print parsing stats (bundle signature + SerializedFile version/unity/metadata counts) for loaded sources, with an optional `--summary` aggregation mode.
- Dependency graph extraction for SerializedFiles (TypeTree + PPtr scan) in `MetadataExtractor`.
- Environment-wide dependency graph builder (best-effort external resolution via `.meta` GUID cache and bundle name heuristics).
- Environment-wide unified object graph (`Environment::build_object_graph`) across YAML + binary sources (best-effort GUID/fileID resolution).
- YAML UI edit helpers (best-effort):
  - Button: set `m_Interactable`, clear/append persistent `onClick` calls (`m_OnClick.m_PersistentCalls.m_Calls`).
  - Canvas: render mode, pixel perfect, sorting flags (overlay-focused).
  - CanvasScaler: scale mode + reference resolution (screen-size workflow).
  - LayoutGroup: padding/alignment/spacing + common child layout toggles.
  - Toggle: set `m_IsOn`/`m_Interactable` and append persistent `onValueChanged` calls.
  - Slider: set value/min/max/wholeNumbers/interactable and append persistent `onValueChanged` calls.
  - Dropdown: set `m_Value`/`m_Interactable` and append persistent `onValueChanged` calls.
  - InputField: set `m_Text`/`m_Interactable` and append persistent `onValueChanged`/`onEndEdit` calls.
  - TMP_InputField: set `m_Text`/`m_Interactable` and append persistent `onValueChanged`/`onEndEdit` calls.
  - ScrollRect: set content/viewport refs, axis toggles, normalized position/velocity, and append persistent `onValueChanged` calls.
  - CanvasGroup: set alpha/interactable/raycast flags.
  - ContentSizeFitter: set horizontal/vertical fit modes.
  - LayoutElement: set sizes + ignoreLayout + layout priority.
  - ToggleGroup: set allowSwitchOff.
  - Scrollbar: set value/size/steps/interactable and append persistent `onValueChanged` calls.
- Additional binary typed helpers (UnityPy-like ergonomics):
  - GameObject: set name (`m_Name`/`name`) and active (`m_IsActive`).
  - Transform: set local position/rotation/scale.
  - RectTransform: set anchored position/size/anchors/pivot/offsets (best-effort).
- Directory-wide `.meta` GUID indexing (`Environment::index_meta_guids_in_directory`) for higher external resolution hit rates without loading every asset file.
- `Environment::set_type_tree_registry_from_paths` to load `.tpk`/`.json` TypeTree registries (best-effort parsing for stripped assets).
- External workflow to generate MonoBehaviour/script TypeTrees via UnityPy + TypeTreeGeneratorAPI:
  - `scripts/export_unitypy_script_typetrees.py` exports a JSON TypeTree registry (`schema: 2`) keyed by `script_id` (Hash128).
  - `docs/SCRIPT_TYPETREES.md` documents export + Rust-side loading.
  - Opt-in E2E test wires the exporter into Rust parsing to validate stripped MonoBehaviour parsing.
- `Environment::load_project` to scan a Unity project root with ignore support and fast binary sniffing (and without loading `.meta` documents by default).
- Graph helpers for analysis and incremental rebuild:
  - `roots` / `leaves` / `cycles` helpers for quick inspection.
  - Rebuild a single source subgraph (`build_dependency_graph_for_source`).
  - Incremental invalidation when reloading sources (`invalidate_dependency_scan_cache_for_source`).
- Search indexing can optionally include AssetBundle `m_Container` asset paths as `kind=BundleContainer` for Everything-style discovery.
- Search daemon flags for container indexing and ignore control (`--search-everything`, `--index-bundle-container-entries`, `--no-gitignore`, `--no-ignore-files`).
- Search daemon `/v1/status` reports best-effort reindex progress (operation + phases + counters) for in-editor UX.
- Search daemon `/v1/reindex` supports `wait=false` to start long reindex jobs asynchronously (recommended for GUI clients).
- Experimental Unity Editor plugin (Asset Hero, UPM package `com.frankorz.asset-hero`, currently in `repo-ref/` only) to start the daemon and provide in-editor search + find references (Unity 2022.3+, UI Toolkit).
- Release automation via `cargo-dist` to ship multi-platform binaries alongside GitHub Releases.
- A manual GitHub Actions workflow to backfill missing dist assets for an existing tag (repair path).

### Changed
- Bundle search/filter helpers now align better with UnityPy-style discovery semantics:
  - `BundleLoader::find_assets_by_name` matches embedded asset names instead of bundle path strings.
  - `BundleLoader::find_assets_by_type` and `BundleProcessor::extract_assets_by_type` filter by actual object presence.

### Fixed
- Metadata reporting:
  - Populate `file_info.compression_type` when extracting from bundles.
  - Fill `ObjectSummary.dependencies` from scanned internal references when enabled.
- YAML serializer: avoid emitting `{...}` placeholders for complex objects when they appear as items in block arrays.
- TypeTree writer: preserve rare unnamed child fields by copying their original byte slices during object rewrites.
- TypeTree writer: normalize `PPtr<>` inputs (`m_FileID/m_PathID` vs `fileID/pathID`, `Null` -> zero pointer).
- SerializedFile (legacy): parse and save `version < 9` layout by seeking metadata at end-of-file (endian boolean prefix) and emitting a compatible save layout.
- More robust external reference resolution by canonicalizing filesystem paths when loading and indexing `.meta` GUIDs.
- Preserve AssetBundle `m_Container` entries with null PPtr (`m_PathID=0`) as unresolved paths instead of dropping them.

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
