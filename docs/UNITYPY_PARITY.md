# UnityPy Parity & Rewrite Tracker

This document tracks the ongoing effort to reach feature parity with the reference UnityPy codebase vendored in this repository.

It is intentionally **implementation-oriented**:
- maps UnityPy modules/functions to Rust modules/APIs,
- defines **milestones** with checkable TODOs,
- documents **acceptance criteria** and **test strategy** so refactors remain auditable.

## Reference Baseline

This repo treats the vendored UnityPy snapshot as the executable specification for edit/write behavior.

- UnityPy version: `1.24.2` (`repo-ref/UnityPy/UnityPy/__init__.py`)
- UnityPy commit: `14f2134c5996a21b15e5fb3ab649e0168e32267d` (`repo-ref/UnityPy`)

## Scope Definition (What “Parity” Means)

Parity targets UnityPy’s **edit pipeline**, not only reading:

1. Load files into an environment (containers + serialized files)
2. Read objects (TypeTree-driven) and allow mutation
3. Write object data back (TypeTree writer)
4. Rebuild `SerializedFile` (metadata + object table + data stream)
5. Rebuild containers (`UnityFS` bundles, `WebFile`)
6. Handle `.resS` / streamed resource payloads
7. Save out to disk with UnityPy-compatible packer options

Non-goals (for now):
- Byte-for-byte identical output vs original inputs (UnityPy itself is not byte-stable either)
- Perfect editor-level semantics for all asset types (parity targets file-format correctness first)

## High-Level Architecture (Rust Target)

To avoid destabilizing the existing parser crates, the write/edit pipeline should live in a **dedicated crate** and integrate into higher-level APIs:

- New crate (recommended): `crates/unity-asset-write`
  - `BinaryWriter` primitives (shared)
  - TypeTree write support
  - `SerializedFile` rebuild/save
  - `BundleFile`/`WebFile` rebuild/save
  - Resource (`.resS`) write & external registration
  - “changed” tracking akin to UnityPy’s `mark_changed()`

Integration:
- `crates/unity-asset` should expose UnityPy-like ergonomic methods (e.g. `Environment::save(...)`) on top of `unity-asset-write`.

## Module Mapping (UnityPy → Rust)

### Environment / Save entrypoint

UnityPy:
- `repo-ref/UnityPy/UnityPy/environment.py`
  - `Environment.save(pack="none", out_path="output")`

Rust (current):
- `crates/unity-asset/src/environment.rs` (load/iterate only, no save)

Rust (target):
- `crates/unity-asset/src/environment.rs`
  - `Environment::save(pack, out_dir)` delegating to `unity-asset-write`

### Object editing hook

UnityPy:
- `repo-ref/UnityPy/UnityPy/classes/Object.py`
  - `Object.save()` → calls `ObjectReader.save_typetree(self)`
- `repo-ref/UnityPy/UnityPy/files/ObjectReader.py`
  - `save_typetree(...)` (writes TypeTree and stores raw bytes)
  - `set_raw_data(...)` + `assets_file.mark_changed()`

Rust (current):
- `crates/unity-asset-binary/src/object.rs`
  - `ObjectHandle` (UnityPy `ObjectReader`-like) is read-only today

Rust (target):
- `crates/unity-asset-write/src/object/serialized_file_session.rs`
  - `SerializedFileEditSession` (UnityPy-like: edit -> save_typetree/set_raw_data -> mark_changed)
  - stores either:
    - parsed properties + TypeTree → encode to raw bytes (`save_typetree` / `edit_object`), or
    - raw byte patch (escape hatch: `set_raw_data`)

### TypeTree read/write

UnityPy:
- `repo-ref/UnityPy/UnityPy/helpers/TypeTreeHelper.py`
  - `read_typetree(...)`
  - `write_typetree(...)` / `write_value(...)`
  - alignment driven by `MetaFlag` (`kAlignBytes`)

Rust (current):
- `crates/unity-asset-binary/src/typetree/serializer.rs`
  - parse + scan fast paths
  - has `serialize_object(...)` but not yet parity-grade (endianness/alignment/edge types)

Rust (target):
- `crates/unity-asset-write/src/typetree/`
  - implement TypeTree-driven writer with strict alignment and full primitive coverage
  - must match UnityPy’s behavior for:
    - `string` (`write_aligned_string`)
    - `TypelessData` (`write_byte_array`)
    - array layout and aligned array element types
    - `pair`, `PPtr<>`, `ReferencedObject`, managed references registry

### SerializedFile save/rebuild

UnityPy:
- `repo-ref/UnityPy/UnityPy/files/SerializedFile.py`
  - `SerializedFile.save(packer=None) -> bytes`
  - rebuilds:
    - header
    - metadata stream (types, object table, scripts, externals, ref types, userInformation)
    - data stream (object bytes), plus alignment rules (8/16 as needed)

Rust (current):
- `crates/unity-asset-binary/src/asset/parser.rs`
  - read-only parse logic, plus lazy object slicing
- `crates/unity-asset-binary/src/asset/header.rs`
  - header parse, including v22 extended fields

Rust (target):
- `crates/unity-asset-write/src/serialized_file/writer.rs`
  - `SerializedFileWriter::save(...) -> Vec<u8>`
  - all version gates mirrored from UnityPy:
    - v>=7 unityVersion
    - v>=8 platform
    - v>=13 enableTypeTree
    - 7<=v<14 bigIdEnabled in metadata
    - v>=11 scriptTypes
    - v>=20 refTypes
    - v>=22 extended header fields and 64-bit offsets
  - data section alignment (notably UnityPy aligns objects’ data stream)

### BundleFile (UnityFS) save/rebuild

UnityPy:
- `repo-ref/UnityPy/UnityPy/files/BundleFile.py`
  - `BundleFile.save(packer=None) -> bytes`
  - `save_fs(...)` for UnityFS:
    - rebuild directory info
    - rebuild and compress block info
    - chunk-based compression for file data
  - supports `packer`: `"none"`, `"original"`, `"lz4"`, `"lzma"`, `(block_info_flag, data_flag)`

Rust (current):
- `crates/unity-asset-binary/src/bundle/*`
  - robust parsing + decompression, including lazy block cache
- `crates/unity-asset-binary/src/compression.rs`
  - decompression support (LZ4/LZMA/Brotli/Gzip) but **no compression** yet

Rust (target):
- `crates/unity-asset-write/src/bundle_write.rs`
  - UnityFS writer that supports the same packer semantics as UnityPy
  - requires implementing compression (LZ4 + LZMA at least) for:
    - blocks info compression
    - data blocks compression
  - must preserve flags and implement `"original"` behavior

### WebFile save/rebuild

UnityPy:
- `repo-ref/UnityPy/UnityPy/files/WebFile.py`
  - `WebFile.save(files=None, packer="none", signature="UnityWebData1.0") -> bytes`
  - supports gzip/brotli/none

Rust (current):
- `crates/unity-asset-binary/src/webfile.rs` (parse only)

Rust (target):
- `crates/unity-asset-write/src/webfile_write.rs`
  - implement save with packer options matching UnityPy
  - requires brotli/gzip **compression** counterparts (decompression already exists)

### Resource files / `.resS` (writeable cab)

UnityPy:
- `repo-ref/UnityPy/UnityPy/files/File.py`
  - `File.get_writeable_cab(...)` creates/returns an `EndianBinaryWriter` for `.resS`
- `repo-ref/UnityPy/UnityPy/files/SerializedFile.py`
  - `SerializedFile.get_writeable_cab(...)` registers it in externals

Rust (current):
- loader can read resources (best-effort export paths exist)

Rust (target):
- `crates/unity-asset-write/src/resources.rs`
  - `WritableCab` abstraction
  - external registration and path conventions aligned with UnityPy:
    - `archive:/{bundle_name}/{cab_name}`

## TODO Milestones (Trackable Checklist)

### M0 — Project scaffolding & public API shape

- [x] Create `crates/unity-asset-write` with minimal public surface
- [x] Define core traits/structs:
  - [x] `ChangeTracker` (UnityPy `mark_changed`)
  - [ ] `EditSession` (per `SerializedFile`)
  - [x] `PackerOptions` (string/tuple parity with UnityPy)
- [x] Add `unity-asset` integration stubs:
  - [x] `Environment::save(pack, out_dir)` implemented (standalone SerializedFile + UnityFS bundle repack)
  - [x] `Environment::edit_binary_object_key(...)` / `EnvironmentEditSession` for UnityPy-like change tracking

Acceptance:
- Compiles with `cargo build --workspace`

### M1 — BinaryWriter primitives (required for all write paths)

- [x] Implement `BinaryWriter` with:
  - [x] endian support (little/big)
  - [x] primitives mirroring UnityPy `EndianBinaryWriter`
  - [x] `align_stream(alignment)`
  - [x] `write_aligned_string`, `write_byte_array`

Acceptance:
- Unit tests for endian + alignment correctness (round-trip with `BinaryReader`)

### M2 — TypeTree write parity

- [x] Implement TypeTree-driven writer matching UnityPy `TypeTreeHelper.write_value`
- [x] Cover types:
  - [x] primitives (`SInt8/UInt8/.../double/bool`)
  - [x] `string`, `TypelessData`
  - [x] `pair` (accepts Array(len=2) + Object(first/second))
  - [x] arrays (including aligned arrays)
  - [ ] `PPtr<>` (TODO: add explicit normalization/acceptance tests)
  - [x] `ReferencedObject` (ref_types-aware)
  - [x] managed references registry (`ManagedReferencesRegistry`) skip rules
- [x] Add targeted fixtures by parsing existing samples and re-serializing a no-op tree

Acceptance:
- For selected objects: parse → write → parse yields equivalent structure (within expected normalization)

### M3 — SerializedFile.save parity

- [x] `SerializedFileWriter.save(...)` implementing UnityPy layout and version gates (v>=9 baseline)
- [x] Object table rebuild:
  - [x] offsets (32/64-bit depending on version)
  - [x] size, type id/index, stripped/script fields
  - [x] alignment (object stream + metadata alignment)
- [x] Support “edited object bytes” overriding original slices (`SerializedFileEdits`)
- [ ] TODO: support `version < 9` save (UnityPy supports older layouts)
- [ ] TODO: legacy TypeTree dump `SerializedType::write_type_tree` for `version == 2`

Acceptance:
- A `.assets` produced by Rust can be loaded by:
  - [x] this Rust parser
  - [ ] UnityPy (baseline snapshot) (TODO: add cross-check integration tests)

### M4 — BundleFile.save (UnityFS) parity

- [x] Implement chunk/block builder like UnityPy `CompressionHelper.chunk_based_compress`
- [x] Implement compression:
  - [x] LZ4 (block mode)
  - [x] LZMA (UnityPy-style header layout: 5-byte header, no unpacked-size field)
  - [ ] TODO: validate LZMA encoder parameters (props/dict) vs UnityPy defaults
- [x] Packer compatibility:
  - [x] `"none"`, `"lz4"`, `"lzma"`, `"original"`, tuple form
- [x] Directory info rebuild and file flags propagation
- [ ] TODO: legacy bundle save (`UnityWeb` / `UnityRaw`)

Acceptance:
- [x] A rebuilt bundle loads in this Rust parser (roundtrip test)
- [ ] TODO: load in UnityPy and compare directory listing (add integration tests)

### M5 — WebFile.save parity

- [x] Rebuild header + file table + data blobs
- [x] Compression parity:
  - [x] gzip compress
  - [x] brotli compress (best-effort; see note below)
- [x] Signature handling (`UnityWebData*` / `TuanjieWebData*`)

Acceptance:
- [x] WebFile loads in Rust parser; extracted sub-files match.
- [x] Uncompressed WebFile output loads in UnityPy.
- [ ] TODO: UnityPy cannot currently re-load brotli-compressed WebFiles produced via `WebFile.save(packer="brotli")` due to its `BROTLI_MAGIC` heuristic; treat brotli as best-effort until upstream behavior changes.

### M6 — `.resS` / streamed resources parity

- [x] Implement `get_writeable_cab` equivalent behavior (P0):
  - [x] create a writable cab buffer inside container (`AssetBundle` / `WebFile`)
  - [x] propagate bundle file flags (best-effort)
  - [x] register as external with GUID and archive path (`archive:/{serialized_name}/{cab_name}`)
- [x] Write standalone sidecar cab files under `out/{asset_file_name}_data/{cab_name}` (best-effort; UnityPy does not expose this for standalone `SerializedFile`)
- [x] Provide an ergonomic helper for streamed-resource fields:
  - [x] `EnvironmentEditSession::write_streamed_resource_to_field(...)` writes bytes into a cab and updates `{path,offset,size}` / `{m_Source,m_Offset,m_Size}` in-place (e.g. `m_StreamData`)
- [x] Provide a generic `PPtr` path helper (Unity-style references):
  - [x] resolve via `Environment::resolve_pptr_path_key(...)`
  - [x] set via `EnvironmentEditSession::set_pptr_path_to_key(...)` (best-effort externals)
- [x] Provide typed convenience helpers for common streamed asset types:
  - [x] AudioClip (`m_Resource`)
  - [x] Texture2D (`m_StreamData`)
  - [x] Mesh (`m_StreamData`)
  - [x] VideoClip (`m_ExternalResources`)
- [x] Provide a typed convenience helper for a common non-streamed edit:
  - [x] TextAsset (`m_Script`)
- [x] Expand typed helpers (UnityPy-like ergonomics):
  - [x] MeshFilter (`m_Mesh`)
  - [x] MeshRenderer (`m_Materials`, `m_AdditionalVertexStreams`)
  - [x] Material:
    - [x] TexEnv texture (`m_SavedProperties.m_TexEnvs[*].m_Texture`) by name
    - [x] TexEnv scale/offset (`m_SavedProperties.m_TexEnvs[*].m_Scale/m_Offset`) by name
    - [x] Floats/Colors/Ints (`m_SavedProperties.m_Floats/m_Colors/m_Ints`) by name
  - [x] VideoPlayer (`m_Url`, `m_VideoClip`)
- [ ] TODO: expand typed helpers further (more classes + deeper editor semantics)

Acceptance:
- [x] A bundle can be modified to point `m_StreamData` at a newly written cab and reloaded.

### M7 — Regression suite (“golden”)

- [x] Add integration tests that shell out to vendored UnityPy (opt-in via env var)
  - [x] UnityFS bundle: Rust save -> UnityPy load OK + directory sanity checks
  - [x] SerializedFile: Rust save -> UnityPy load OK + objects non-empty
  - [x] Rust TypeTree edit (rename a `m_Name`/`name` field) -> repack bundle -> UnityPy observes mutation
- [ ] Add corpus-driven tests with `tests/samples/*`:
  - [x] bundle round-trip (no-op save)
  - [x] serialized file round-trip

Acceptance:
- `cargo nextest run --workspace` passes on CI/dev machines (with a clearly documented UnityPy test dependency toggle)

How to run UnityPy E2E checks locally:

1) Create or point to a python that can import UnityPy dependencies (recommended: `.venv-unitypy`).
2) Run:

```
$env:UNITYPY_E2E = "1"
# optional, if not using `.venv-unitypy`:
$env:UNITYPY_PYTHON = "C:\\path\\to\\python.exe"
cargo nextest run -p unity-asset-write unitypy_
```

How to run external AssetBundle E2E edits locally (no samples checked into repo):

This repo includes opt-in tests that can edit and repack an *external* AssetBundle, then optionally ask UnityPy to validate the result.

1) Point the test at an existing bundle on your machine (the test writes to a temp output dir; it does not modify the input file):

```
$env:UNITY_ASSET_EXTERNAL_BUNDLE = "C:\\path\\to\\bundle.ab"
```

2) Run the Rust-side E2E:

```
cargo nextest run -p unity-asset external_bundle_
```

3) Optional: enable UnityPy validation (requires `repo-ref/UnityPy` and a working python env):

```
$env:UNITYPY_E2E = "1"
# optional, if not using `.venv-unitypy`:
$env:UNITYPY_PYTHON = "C:\\path\\to\\python.exe"
cargo nextest run -p unity-asset external_bundle_
```

## Risk Register (Known Hard Parts)

- Unity version branching: header formats (`<9`, `>=9`, `>=22`) and object table changes.
- Endianness: big-endian assets exist; writer must honor `header.endian`.
- TypeTree stripped assets (`enableTypeTree=false`): requires registry-based node reconstruction or limited editing modes.
- Managed references (`ReferencedObject`, `ManagedReferencesRegistry`) correctness.
- Compression: UnityFS chunking and flags semantics (`original` mode) must match UnityPy expectations.
- `.resS` coupling: external file registration and consistent cab naming/path rules.

## Notes (Implementation Principles)

- Prefer correctness and spec parity over micro-optimizations in write path.
- Keep read-only parser crates stable; put new mutation/write behavior behind a separate crate and explicit APIs.
- Every milestone must add tests that prevent regressions in subsequent refactors.
