# Roadmap

The project is becoming an agent-friendly Unity asset engine: every supported action should be
discoverable, accept structured input, produce versioned output, and preserve enough evidence for
another process to verify or recover it.

## Current Foundation

The following architecture is implemented:

- one authoritative `AssetWorkspace` with immutable committed and prepared views;
- stable source locators, object addresses, workspace revisions, and content digests;
- `WorkspaceInspector` source/object contracts and exact streamed-resource resolution;
- deterministic `MutationPlan` v2, zero-write prepare, prepared preview, recoverable commit, and
  typed recovery outcomes;
- one revision-bound `ReferenceGraph` for YAML and binary references;
- typed extraction request, plan, manifest, and report contracts;
- a local `/v2` search service with search, suggestions, reverse references, status, authenticated
  reindex, and token rotation;
- a machine-readable workspace capability catalog for agents and transport adapters.

## Priority 1: Agent-Native Workspace Tools

- Expose every user-visible workspace operation through a typed contract before adding convenience
  text commands.
- Add contract fixtures and generated client examples for Rust, JSON CLI, and future tool
  transports.
- Improve capability discovery with representation, recipe, and source-specific rejection
  details.
- Keep display text out of request parsing; stable IDs, locators, addresses, revisions, and
  transaction IDs are the automation boundary.
- Add compact inspection filters and pagination only with deterministic ordering and explicit
  completeness metadata.

Success means an agent can discover support, inspect state, prepare a guarded change, preview it,
commit it, and handle recovery without scraping logs.

## Priority 2: Binary-Aware Project Indexing

- Ingest workspace `ChangeSet` values as the incremental boundary for derived search generations.
- Preserve generation revision, transaction ID, coverage, diagnostics, and failed-source state in
  search status.
- Expand binary indexing from names, TypeTree PPtr facts, and bundle container paths to selected
  class-specific fields with bounded extraction.
- Add object-address and field-path navigation data without making the search daemon authoritative
  for asset bytes.
- Measure warm query latency, initial indexing time, incremental reconciliation, peak memory, and
  index size on published corpora.

The recommended design remains a consumer-owned local index. Scanning every query or embedding
mutable index state inside `AssetWorkspace` would couple authoritative asset state to a replaceable
read model.

## Priority 3: Semantic Mutation Recipes

- Extend guarded recipes for common game-development workflows based on observed YAML and binary
  schemas.
- Add capability inspection for each recipe variant and structured reasons for rejection.
- Coordinate object metadata and streamed-resource artifacts through one mutation plan.
- Preserve exact schema provenance and lower every recipe to generic mutation operations.

Recipe breadth should grow from corpus-backed use cases. A new class helper is not complete until
the prepared view, written artifact, reopened workspace, reference graph, and change set agree.

## Priority 4: Extraction and Decode

- Add representations for high-value textures, sprites, audio, meshes, text, and metadata while
  keeping heavy codecs optional.
- Carry representation provenance, dependency closure, output digest, and diagnostics through the
  extraction manifest.
- Improve resumable extraction for large artifact sets and validate resource ceilings with
  deterministic characterization fixtures.
- Add explicit fallbacks when an exact decoded representation is unavailable; never relabel raw
  bytes as a successful decode.

## Priority 5: Compatibility Corpus

- Expand SerializedFile and container coverage across Unity versions, platforms, endian modes, and
  regional engine forks.
- Add managed and IL2CPP script-schema corpora before implementing a native generator.
- Define encrypted-bundle support only after decryption and output policy can be tested safely.
- Keep optional external-reader validation, but retain Rust wire tests and independent reparsing as
  the release authority.

## Priority 6: Editor and Tool Integration

- Keep Unity Editor integration as a thin client of the local search and workspace contracts.
- Support asset navigation first, then object-address and hierarchy navigation where evidence is
  available.
- Package daemon binaries with an explicit protocol compatibility matrix.
- Add transport adapters only when they preserve the same typed inputs, outputs, budgets, and
  transaction semantics as the Rust API.

## Non-Goals

- a generic string command bus;
- implicit dependency loading or filesystem probing during reference resolution;
- mutable object handles that bypass revision guards;
- search-index ownership inside the authoritative workspace;
- publication without independent prepare evidence;
- silent compatibility fallbacks for encryption, schemas, or decoded representations.
