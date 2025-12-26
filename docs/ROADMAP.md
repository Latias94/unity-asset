# Roadmap

This document tracks roadmap items at a feature level. It is intentionally high-level and focuses on user-visible outcomes.

## Search daemon (Search Everything)

Goal: IDE-like "Search Everything" experience for Unity projects (assets, scenes/prefabs hierarchy, and editor actions), with strong fuzzy matching and fast incremental indexing for large repositories.

### MVP (Phase 0): Useful immediately, even on large projects

- Deliver a local daemon process (`localhost` API) that can:
  - Index and search `path`, `filename`, `asset type`, `labels` (Tier-0).
  - Provide fast results with stable ranking and highlighting.
  - Incrementally update the index for changed files using fingerprints.
- Provide a CLI client for smoke testing and scripting.

Success criteria:

- Search latency: P95 < 50ms for typical queries on a warm index.
- Cold-start usability: initial Tier-0 index is available quickly, even if deeper indexing is still running.
- Large projects: incremental update completes within seconds for small change sets.

### Phase 1: Better ranking, suggestions, and query language

- Query syntax: `t:prefab in:Assets/UI c:MeshRenderer "Start Button"`.
- Field weights and predictable ordering.
- Suggestions/autocomplete for:
  - common paths
  - types
  - component/script names (when available)

### Phase 2: Scene/Prefab semantics (Tier-1 extraction)

- Extract and index:
  - GameObject names and hierarchy paths
  - component types
  - script GUIDs (MonoBehaviour `m_Script`)
- Show results with hierarchy paths without opening scenes.

Current status:

- Implemented: basic Tier-1 signals (YAML `m_Name`, tags, `{guid, fileID}` occurrences) so prefab/scene/object searches become useful quickly.
- Implemented: resolve `m_Script guid` to script `path` + best-effort `namespace/class` terms, so searching by component/script names returns scenes/prefabs.
- Implemented: best-effort prefab/scene hierarchy path terms (`Root/Child/...`) for searching.
- Pending: richer component extraction (built-in components), and result rendering with object paths.

### Phase 3: Find References (reverse edges)

- Build a reverse reference index:
  - `target (guid, fileID?) -> sources[]`
- Provide results with context:
  - source asset path/type
  - best-effort component + field name
  - best-effort hierarchy path (scene/prefab)

Current status:

- Implemented (YAML): daemon endpoint `GET /v1/references?guid=...&file_id=...` returns source files that reference the target GUID (best-effort for PPtr-like `{guid,fileID}` blocks).
- Implemented (binary): best-effort reverse references for `SerializedFile` / `AssetBundle` by scanning TypeTree `PPtr` (`fileID`, `pathID`) and mapping externals to GUIDs when available.
- Pending: richer field-level context, result grouping/deduplication, and Unity Editor jump-to-location.

### Phase 4: Deep mode (Tier-2 decode) and non-text assets

- On-demand deep extraction using binary decoding for richer context.
- Optional offline indexing for bundles or player data (scope to be defined).

### Phase 5: Unity integration

- Unity Editor UI:
  - quick search popup
  - navigate to assets and (best-effort) object locations
  - actions provider (menu/commands)

Notes:

- The daemon and Unity UI should remain decoupled; the daemon is the single source of truth for indexing and querying.
