# ADR 0003: Unity Editor plugin integration and repository strategy

- Status: Proposed
- Date: 2025-12-26

## Context

`unity-asset-search-daemon` provides a local "Search Everything" experience via an HTTP API. To make this useful for Unity users, the best UX is a Unity Editor plugin that:

- starts/monitors the daemon for the current project,
- queries it for search/references results,
- navigates to assets (and later object-level locations) inside the Editor.

The Rust workspace is currently optimized for Rust development. Unity plugin development has different constraints:

- Unity projects generate many `.meta` files and platform-specific artifacts,
- package distribution typically follows UPM (Unity Package Manager) conventions,
- shipping native binaries for Windows/macOS/Linux requires release orchestration.

## Decision

### 1) Keep the Unity plugin in a separate repository (template repo)

Create a dedicated repository, e.g.:

- `unity-asset-unity` (recommended name), or
- `unity-asset-search-unity`

This repository is a UPM package template and is versioned independently from the Rust workspace, while still tracking compatible daemon versions.

Rationale:

- avoids polluting the Rust workspace with Unity-specific files and `.meta` churn,
- allows Unity-specific CI (UPM packaging, editor tests, platform packaging),
- supports Unity users who do not use Rust.

### 2) Plugin communicates with the daemon over localhost HTTP

The Unity plugin uses:

- `GET /v1/health`
- `GET /v1/status`
- `GET /v1/search?q=...&limit=...`
- `GET /v1/suggest?prefix=...&limit=...`
- `GET /v1/references?guid=...&file_id=...&limit=...`
- `POST /v1/reindex?...` (authenticated via token)

The daemon stays localhost-only by default. Authorization uses the existing bearer token file stored in the index directory.

### 3) Process management inside Unity Editor

The plugin owns a small "daemon manager" layer:

- Determine `project_root` as Unity project directory.
- Choose `index_dir`:
  - default to `Library/unity-asset-search` (Unity's recommended cache location),
  - allow override in plugin settings.
- Start the daemon process if missing.
- Keep a single instance per project:
  - store pid/port/token info under `Library/unity-asset-search/`.

### 4) Packaging strategy for daemon binaries

MVP options (in increasing UX quality):

1. Developer mode: require `cargo install unity-asset-search-daemon` and configure the executable path in Unity preferences.
2. Recommended: ship prebuilt binaries inside the UPM package:
   - `Tools/<platform>/unity-asset-search-daemon[.exe]`
   - `Tools/<platform>/unity-asset-search-cli[.exe]` (optional; useful for debugging)
3. Optional: on first run, download a matching binary from GitHub Releases (requires network and stronger security story).

For reliability and offline use, option (2) is preferred for production.

### 5) Navigation scope for MVP

MVP navigation supports:

- open/ping asset by `Location.path` (Unity `AssetDatabase` path),
- open/ping references sources (same).

Follow-up work can add object-level navigation:

- prefabs/scenes: use `fileID` and extracted hierarchy paths to locate objects,
- serialized files: map `pathID` to object handles for richer inspection.

## Consequences

Pros:

- clear separation of concerns: Rust workspace (engine) vs Unity repo (product UX),
- cleaner CI and release pipelines per ecosystem,
- easier onboarding for Unity users.

Cons:

- requires cross-repo version compatibility policy,
- needs release automation to bundle daemon binaries into the Unity package.

## Alternatives considered

### A) Keep the Unity plugin inside this repository

Pros: single repo, easier to coordinate changes.

Cons:

- Unity `.meta` churn and package assets increase noise and maintenance costs,
- CI becomes more complex (Unity + Rust toolchains),
- contributors without Unity installed have a worse experience.

### B) Use a pure ripgrep-based Unity plugin (no daemon)

Pros: extremely simple and fast for GUID reference search.

Cons:

- cannot provide object-level indexing, ranking, and richer queries,
- becomes hard to extend into "Search Everything" beyond GUID text search.

