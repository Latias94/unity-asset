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
- Tier-2 (future generation build): optional deep decoding
  - best-effort enrichment (object name/type/field context) is projected before publication

Queries never reopen Unity sources for enrichment. Every result is answered from one immutable
Search Generation so its fields, references, and revision stamp cannot come from different source
snapshots.

Incremental indexing has two distinct ownership paths. The filesystem daemon admits only full,
reconcile, and changed-path requests. Authoritative workspace consumers call the search-index
library with revision-bound `ChangeSet` values after committing workspace state. A committed search
generation records one coherent view of documents, reverse references, and status. Reindex receipts
separate assets that were analyzed from assets considered during dependency discovery. Until the
dependency projection has a persistent reverse index, changed-path and `ChangeSet` builds explicitly
report a full cached dependency scan and its candidate count; unchanged source bytes remain closed.

The append-only activation log is also the durable generation-head protocol. Once the index
observes a target workspace revision, it appends a head that keeps the current immutable generation
as `actual_revision` and records the target as `desired_revision`. A failed build therefore remains
stale across process restart. Activating a complete replacement generation records
`actual_revision == desired_revision` at the same commit point. Detailed failure text remains a
bounded process-local diagnostic rather than a second persisted authority. The highest committed
head is the sole freshness authority. If that head or its immutable generation is corrupt, opening
fails closed instead of silently falling back to an older head and reporting stale data as fresh.
Corrupt lower history does not prevent opening a valid latest head.

The daemon performs startup reconciliation and an independent periodic reconciliation sweep. File
watching only reduces update latency; it is not the recovery mechanism for missed events or a
transient failed build.

### Storage location

- Default: per-project index under Unity's `Library/` folder (not versioned, safe to delete).
- For non-Unity use: a per-workspace cache directory (to be defined), with a deterministic mapping from project root to index path.

### API shape

- Bind to `127.0.0.1` only.
- Require a per-project token for mutation endpoints.
- Reject unknown fields, methods, and contract versions.
- Core endpoints:
  - `GET /v2/health`
  - `GET /v2/search`
  - `GET /v2/suggest`
  - `POST /v2/references` with a versioned `ReferenceRequest`
  - `GET /v2/status`
  - `POST /v2/reindex` with a versioned filesystem reindex intent
  - `POST /v2/token/rotate`

### Implementation split (workspace crates)

Create dedicated crates to keep concerns separated:

- `unity-asset-search-core`: query DSL, schema, tokenization, ranking policy (no IO)
- `unity-asset-search-index`: index backend + incremental pipeline
- `unity-asset-search-daemon`: HTTP server + orchestration
- `unity-asset-search-cli`: developer-facing client

The workspace and parsing crates remain authoritative for deep extraction and binary-specific
metadata. The daemon owns only the derived search generation.

The daemon does not expose a workspace transaction queue. Its HTTP and coordinator types cannot
represent `ChangeSet`; clients that own an authoritative workspace snapshot use the
`unity-asset-search-index` library boundary directly. A bare cross-process `ChangeSet` contains no
source bytes, Source Catalog, or parse context, so it cannot prove an arbitrary historical target
revision after the filesystem advances again. Reconciliation is the honest cross-process recovery
path. A future remote transaction endpoint would require a content-addressed snapshot locator or
would have to reject targets that the daemon cannot reproduce exactly from its current filesystem.

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

- Tier-0 path, name, type, query, suggestion, and incremental reconciliation are implemented.
- Tier-1 YAML names, tags, script terms, hierarchy paths, and reference facts are implemented on a
  best-effort basis.
- Binary PPtr facts and optional AssetBundle container paths can contribute to the index.
- `/v2` request and response contracts, bearer-token reindexing, token rotation, and coordinator
  status are implemented.
- Richer binary field extraction and exact editor object navigation remain roadmap work.
