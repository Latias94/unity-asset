# Refactoring Plan (UnityPy-aligned)

This document describes a **fearless refactor** roadmap for this repository, inspired by UnityPy’s
architecture and field-proven parsing strategy, while taking advantage of Rust’s safety and
performance strengths.

## Goals

- **Correctness first**: prioritize format correctness (UnityFS flags, SerializedFile v22+ headers),
  deterministic parsing behavior, and predictable failure modes.
- **Security hardening**: eliminate resource exhaustion risks (unbounded allocations, unchecked
  arithmetic, unchecked seek).
- **Stable abstractions**: converge to a clear 3-layer model:
  1) low-level parsers (structure-only, slice-based)
  2) object handles (lightweight readers / context)
  3) high-level environment (indexing, caching, export)
- **Scalable performance**: avoid unnecessary copies, add lazy indices and lazy string resolution,
  enable safe parallel extraction workflows.
- **API discipline**: reduce public surface area, separate experimental APIs, and prepare for SemVer.

## Non-goals (for this refactor wave)

- Full UnityPy parity across all object types and versions.
- A perfect, zero-copy end-to-end pipeline in one step.
- Keeping the existing public API stable. This refactor is allowed to be breaking.

## Current Pain Points (Observed)

- **BinaryReader string API**: length-prefixed strings are currently read as `u32` and used for
  direct allocation, which is unsafe for hostile inputs.
- **SerializedFileHeader**: v22+ uses 64-bit fields (`file_size`, `data_offset`), but the current
  implementation truncates them to `u32`.
- **UnityFS flags handling**: `BlocksInfoAtEnd` is ignored and replaced by a “temporary fix”, which
  will fail on some real-world bundles.
- **TypeTree parsing strategy**: “lenient by default” behavior exists, but warnings are printed to
  stderr from library code. Consumers can’t control strictness or collect warnings.
- **Object lookup**: `SerializedFile::find_object` is currently O(n); UnityPy uses a dict keyed by
  `path_id`.
- **Public API surface**: `unity-asset-binary` re-exports many items at the top level, making SemVer
  hard to control.
- **Heavy default features**: decode/processing features are enabled by default, making the core
  parser heavier than needed.
- **Non-thread-safe caches**: `Environment` uses `RefCell` caches, complicating concurrency.

## Target Architecture (UnityPy-aligned)

UnityPy’s model can be summarized as:

- **File model**: `File` → (`BundleFile` / `SerializedFile` / `WebFile`)
- **Object handle**: `ObjectReader` is a lightweight handle (offset, size, type, context)
- **High-level APIs**: environment, indexing, export, discovery

The Rust target design should mirror this:

### 1) Low-level parsers (formats)

- `formats::bundle` parses headers, blocks, directory; returns slice-based structures.
- `formats::serialized` parses headers, types, objects; returns object metadata + shared data buffer.
- `formats::web` parses UnityWeb/UnityRaw containers.

Design rules:

- No logging in parsers.
- No decoding/export in parsers.
- Avoid copying data; prefer `Arc<[u8]>` and slicing.

### 2) Object handle layer

- Introduce an `ObjectHandle` (or `ObjectRef`) that keeps:
  - source context (serialized file handle)
  - `path_id`, `type_id`
  - `byte_start`, `byte_size`
- Parsing into `UnityObject` becomes `handle.read()` with configurable strictness.

### 3) High-level environment

- Environment manages:
  - loading multiple sources
  - indices (by path_id, by container path)
  - caches (TypeTree, container extraction, streamed resources)
  - export/decode workflows (via dedicated “decode” layer)

## Parsing Modes (Strict vs Lenient)

We adopt two user-selectable parsing modes:

- **Strict**: fail-fast on any structural mismatch, overflow, or unexpected EOF.
- **Lenient**: best-effort parsing; collect warnings and return partial results where possible.

Rules:

- No `eprintln!` inside libraries.
- Warnings should be returned to the caller or recorded via a configurable collector.

## Work Plan

### Phase 0 — Safety & Correctness (mandatory)

- Harden `BinaryReader` string reads with bounds and limits.
- Fix `SerializedFileHeader` v22+ to store 64-bit values without truncation.
- Fix endian seek for v<9 using `checked_sub`.
- Implement UnityFS `BlocksInfoAtEnd` properly (seek to end when flagged).
- Use explicit uncompressed size for UnityWeb where available.
- Add TypeTree parse options (strict/lenient) and structured warnings (no stderr logging).

Deliverables:

- passing unit tests for new bounds checks
- a few golden samples (ignored by default) for bundle variants

### Phase 1 — Indices & Copy Elimination (high ROI)

- Add lazy `path_id` → object index (`HashMap<i64, usize>`) for `SerializedFile`.
- Remove bundle `data.clone()` patterns; move to `Arc<[u8]>` or scope-limited borrowing.
- Reduce String cloning in hot paths where possible.

### Phase 2 — Layered Public API

- Replace large top-level re-exports with layered modules:
  - `unity_asset_binary::formats::*`
  - `unity_asset_binary::object::*`
  - `unity_asset_binary::extractors::*` (optional)
  - `unity_asset_binary::experimental::*`
- Decide SemVer commitments per module.

### Phase 3 — Decode/Export Separation

- Move decoding/export logic (Texture/Audio/Mesh/Sprite) into `unity-asset-decode` (or similar).
- Keep `unity-asset-binary` minimal by default; decoding is opt-in.

### Phase 4 — Concurrency-ready Environment

- Replace `RefCell` caches with `RwLock`/`DashMap`.
- Make cache invalidation explicit and testable.

## Migration Strategy

Because this is a fearless refactor, we can choose one of:

- **Breaking release**: perform API breakage in a single minor-to-major bump, update all workspace
  crates in lockstep.
- **Soft migration**: keep old APIs behind `deprecated` wrappers and introduce new APIs in parallel.

For now, we recommend **breaking release** for Phase 0–2, then stabilize.

## Success Criteria

- No unchecked arithmetic in header parsing.
- No unbounded allocations from hostile inputs.
- UnityFS flags are honored (especially `BlocksInfoAtEnd`).
- TypeTree errors are controllable and observable (no stderr logging).
- Object lookup is near O(1) by default after first query.
- Clear module boundaries: parsing vs handles vs environment vs decode.

