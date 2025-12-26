# Examples

This repository maintains runnable examples per crate (built in CI).

## Crate Guide

- `unity-asset` (library): main user-facing API. Use this if you want an `Environment` that can load YAML + binary sources and iterate objects across bundles/serialized files/webfiles.
- Examples live in `crates/unity-asset/examples/`.
- `unity-asset-binary` (parser): low-level binary parsers (AssetBundle / SerializedFile / WebFile) plus fast helpers (`sniff_*`, `ObjectHandle::peek_name`, `ObjectHandle::scan_pptrs`).
  - Examples live in `crates/unity-asset-binary/examples/`.
- `unity-asset-yaml` (YAML): Unity YAML parsing/serialization utilities. Most users can access this via `unity-asset::YamlDocument`.
- `unity-asset-decode` (decode/export): optional heavier decode/export helpers (Texture/Audio/Sprite/Mesh) behind feature flags.
  - Examples live in `crates/unity-asset-decode/examples/`.
- `unity-asset-cli` (CLI): command-line tools. Not needed for library integration.
- `unity-asset-search-daemon` (experimental): local "Search Everything" daemon (`localhost` HTTP API).
- `unity-asset-search-cli` (experimental): CLI client for the search daemon (search/status/suggest/reindex/bench).

## unity-asset (library)

- YAML load summary:
  - `cargo run -p unity-asset --example yaml_load_summary`
- Environment load + list:
  - `cargo run -p unity-asset --example env_load_and_list -- tests/samples`
- Load a Unity project root (index `.meta` GUIDs + scan binaries, skip `.meta` documents by default):
  - `cargo run -p unity-asset --example env_load_and_list -- repo-ref/BoatAttack` (or use `Environment::load_project` in code)
- Build a project-wide object graph (fast project scan + graph build):
  - Binaries only (recommended for big projects):
    - `cargo run -p unity-asset --example env_project_object_graph -- repo-ref/BoatAttack`
  - Include YAML documents too (heavier):
    - `cargo run -p unity-asset --example env_project_object_graph -- repo-ref/BoatAttack yaml`
  - Print DOT:
    - `cargo run -p unity-asset --example env_project_object_graph -- repo-ref/BoatAttack yaml dot > graph.dot`
- Bundle container lookup (UnityPy-like discovery):
  - `cargo run -p unity-asset --example env_container_lookup -- tests/samples Assets/`
- Build an Environment-wide dependency graph (TypeTree required for edges):
  - `cargo run -p unity-asset --example env_dependency_graph -- tests/samples/char_118_yuki.ab "Assets/*"`
  - Print DOT (includes resolved external edges):
    - `cargo run -p unity-asset --example env_dependency_graph -- tests/samples/char_118_yuki.ab "Assets/*" dot > graph.dot`
  - Incremental rebuild (core API):
    - Use `Environment::build_dependency_graph_for_source` plus `Environment::invalidate_dependency_scan_cache_for_source` when reloading a single bundle entry.
- Build a unified object graph (YAML + binary, best-effort resolution):
  - `cargo run -p unity-asset --example env_object_graph -- crates/unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab`
  - Print DOT:
    - `cargo run -p unity-asset --example env_object_graph -- crates/unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab dot > graph.dot`
- Find by `path_id` and dump JSON:
  - `cargo run -p unity-asset --example env_find_and_dump -- <path> <path_id>`
- Export a stable binary object index (JSONL):
  - `cargo run -p unity-asset --example env_export_index_jsonl -- <path> [limit]`
- Read streamed resource bytes (m_Resource / m_StreamData):
  - `cargo run -p unity-asset --example env_read_stream_data -- <path> [path_id]`
- List WebFile entries (UnityWebData* containers):
  - `cargo run -p unity-asset --example env_webfile_list_entries -- <path-to-UnityWebData>`

## unity-asset-binary (parser)

- Sniff file kind from a prefix:
  - `cargo run -p unity-asset-binary --example sniff_kind -- tests/samples/char_118_yuki.ab`
- Load and print summary:
  - `cargo run -p unity-asset-binary --example load_and_list -- tests/samples/char_118_yuki.ab`
- Scan `PPtr` references (TypeTree required):
  - `cargo run -p unity-asset-binary --example scan_pptrs -- <path> <path_id> [asset_index]`
- JSON TypeTree registry for stripped assets:
  - `cargo run -p unity-asset-binary --example typetree_registry_json -- <path>`

## unity-asset-decode (optional decode/export)

- Export Texture2D PNGs:
  - `cargo run -p unity-asset-decode --example export_textures --features texture -- tests/samples/char_118_yuki.ab target/out`

## unity-asset-search (experimental)

- Install the tools:
  - `cargo install unity-asset-search-daemon`
  - `cargo install unity-asset-search-cli`
- Start the daemon (auto reindex on first run):
  - `cargo run -p unity-asset-search-daemon -- --project-root repo-ref/BoatAttack --watch`
- Exclude paths (recommended):
  - Use `.gitignore` (supported) or `.ignore` (supported), or add a `.unity-asset-search-ignore` file at project root for tool-specific ignores.
- Query from the CLI:
  - `cargo run -p unity-asset-search-cli -- health`
  - `cargo run -p unity-asset-search-cli -- search "player" --limit 20`
  - `cargo run -p unity-asset-search-cli -- search "PlayerController" --limit 20`
  - `cargo run -p unity-asset-search-cli -- search "UI StartButton" --limit 20`
  - `cargo run -p unity-asset-search-cli -- suggest "t:pr" --limit 10`
  - `cargo run -p unity-asset-search-cli -- status`
  - Find references by GUID:
    - `cargo run -p unity-asset-search-cli -- references deadbeefdeadbeefdeadbeefdeadbeef --limit 50`
    - `cargo run -p unity-asset-search-cli -- references deadbeefdeadbeefdeadbeefdeadbeef --file-id 11500000 --limit 50` (YAML `fileID` / binary `pathID`)
    - The response includes `hits[].stable_id` + `hits[].location` (for navigation) and `hits[].objects[]` (Rider-like object grouping with `field_hints[]`).
- Run the BoatAttack benchmark harness:
  - `scripts/bench_boat_attack.zsh repo-ref/BoatAttack`
- Stress test incremental watcher indexing (burst changes):
  - `scripts/stress_incremental_watch.zsh repo-ref/BoatAttack`
- Stress test watcher-driven directory rename/move:
  - `scripts/stress_rename_watch.zsh repo-ref/BoatAttack`
- Stress test watcher-driven git checkout / branch switching:
  - `scripts/stress_git_checkout_watch.zsh repo-ref/BoatAttack`
  - The script prints a JSON summary (p50/p95/max) and counts how many times the daemon fell back to a full scan due to watcher storms.
  - If the project is large, you may want to lower debounce or tune the fallback threshold:
    - `DEBOUNCE_MS=200 scripts/stress_git_checkout_watch.zsh repo-ref/BoatAttack`
    - `cargo run -p unity-asset-search-daemon -- --project-root repo-ref/BoatAttack --watch --watch-debounce-ms 200 --watch-full-scan-threshold 5000 --watch-reconcile-interval-ms 300000`
  - Note: when watcher storms trigger a fallback, the daemon runs a sharded full scan (one `scan_root` at a time) to reduce I/O on large projects.
