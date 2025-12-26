# ADR 0001: Local search daemon for "Search Everything"

- Status: Accepted
- Date: 2025-12-26

## Context

We want an IDE-like "Search Everything" workflow for Unity projects:

- fast interactive search ("type to search")
- predictable ranking and good fuzzy matching
- large projects (hundreds of thousands of assets) must remain usable
- results should span assets, scene/prefab hierarchy, and editor actions
- the system should be reusable by multiple clients (Unity editor UI, CLI, other tools)

Unity projects are challenging because:

- references are not just GUID text matches; Unity uses `{guid, fileID}` object references
- full semantic extraction for all assets is expensive and should not block usability

## Decision

Build a local, per-project search daemon and keep the Unity UI as a thin client.

### High-level architecture

- A daemon process (single-writer) owns indexing, query execution, and caching.
- Clients connect via `localhost` API:
  - Unity editor integration (search popup / navigation)
  - CLI client (debugging, scripts, CI)
  - future external tools

### Index strategy: tiered and incremental

The index is built in tiers to keep cold-start acceptable:

- Tier-0 (immediate): asset metadata
  - `guid`, `path`, `filename`, `type`, `labels`, `mtime`, `size`
- Tier-1 (background): YAML-focused semantic extraction
  - GameObject names and hierarchy paths (prefab/scene)
  - component types
  - key fields (tag/layer) and a small set of user-visible strings
  - reference edges (PPtr-like `{guid, fileID}` occurrences)
- Tier-2 (on-demand): deep decoding
  - best-effort enrichment (object name/type/field context) for a small set of results or "deep mode"

Incremental indexing is driven by fingerprints per GUID (e.g. `mtime/size/hash`), so the steady-state cost is proportional to the change set, not project size.

### Storage location

- Default: per-project index under Unity's `Library/` folder (not versioned, safe to delete).
- For non-Unity use: a per-workspace cache directory (to be defined), with a deterministic mapping from project root to index path.

### API shape

- Bind to `127.0.0.1` only.
- Require a per-project token for write or reindex endpoints.
- Core endpoints:
  - `GET /v1/search`
  - `GET /v1/suggest`
  - `GET /v1/status` (index progress, versions, last update)
  - `POST /v1/reindex` (scoped reindex)

### Implementation split (workspace crates)

Create dedicated crates to keep concerns separated:

- `unity-asset-search-core`: query DSL, schema, tokenization, ranking policy (no IO)
- `unity-asset-search-index`: index backend + incremental pipeline
- `unity-asset-search-daemon`: HTTP server + orchestration
- `unity-asset-search-cli`: developer-facing client

The existing parsing crates remain the source for deep extraction (Tier-2) and any binary-specific metadata.

## Consequences

- Pros:
  - Interactive latency is predictable because the index is local and warmed.
  - Unity stays responsive; indexing is out-of-process.
  - Multiple clients can reuse the same indexing and query implementation.
  - Tiered design keeps cold-start acceptable while allowing a high ceiling.
- Cons / costs:
  - Additional deployment complexity (daemon lifecycle, versioning, upgrades).
  - Index storage and migrations need careful versioning.
  - Tantivy-like backends may require tuning (segment merges, disk usage).

## Alternatives considered

1. Pure in-editor scanning (AssetDatabase + on-demand searches)
   - Too slow and too coupled to Unity's main thread for large projects.
2. `ripgrep`-style GUID scanning
   - Fast to implement, but lacks object-level context and stable ranking, and scales poorly with repeated interactive queries.
3. SQLite FTS5 instead of Tantivy
   - Simpler operationally, but less flexible for advanced ranking, suggestions, and search features expected from an IDE-like experience.

## Implementation status

- Tier-0 shipped (path/name/type indexing).
- Tier-1 started (basic YAML extraction: `m_Name`, tags, `{guid, fileID}` references; script GUIDs are resolved to best-effort C# class/namespace terms for searching).
