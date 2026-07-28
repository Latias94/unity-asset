# Examples

The checked-in examples are compiled in CI. The high-level library and CLI use the same
revision-bound contracts, so an agent can discover capabilities through JSON and then choose a
typed operation without parsing display text.

## Crate Guide

- `unity-asset`: high-level `AssetWorkspace`, immutable views, inspection, mutation, references,
  extraction planning, and recovery.
- `unity-asset-cli`: typed JSON-facing commands for the same workspace workflows.
- `unity-asset-binary`: low-level binary parsing and canonical TypeTree wire execution.
- `unity-asset-yaml`: Unity YAML parsing and serialization.
- `unity-asset-decode`: optional Texture2D, Sprite, and AudioClip representations.
- `unity-asset-search-daemon` and `unity-asset-search-cli`: a consumer-owned local search read
  model and its client.

## Workspace Library

List every loaded source, including exact nested container members and streamed-resource
sidecars:

```powershell
cargo run -p unity-asset --example workspace_source_inventory -- D:\GameProject\Assets
```

Inspect YAML and SerializedFile objects as versioned JSON Lines. A signed binary `path_id` and an
output limit are optional:

```powershell
cargo run -p unity-asset --example workspace_object_inspection -- D:\GameProject 0 100
```

Build one revision-bound `ReferenceGraph` and emit a deterministic JSON, JSON Lines, or DOT
projection:

```powershell
cargo run -p unity-asset --example workspace_reference_graph -- D:\GameProject jsonl
cargo run -p unity-asset --example workspace_reference_graph -- D:\GameProject dot > graph.dot
```

Query every exact `AssetBundle.m_Container` occurrence matching a pattern:

```powershell
cargo run -p unity-asset --example bundle_container_query -- D:\GameProject\game.bundle "Assets/UI/*"
```

Resolve a streamed-resource range from sources already loaded into the snapshot. Resolution never
probes the filesystem for an undeclared sidecar:

```powershell
cargo run -p unity-asset --example workspace_streamed_resource -- D:\GameProject\game.bundle -42
```

The YAML-only adapter remains useful for small tools:

```powershell
cargo run -p unity-asset --example yaml_load_summary -- D:\GameProject\Assets\Scene.prefab
```

## Typed Workspace CLI

All successful machine-facing commands write one JSON document to stdout. Failures write the
versioned `unity_asset.cli_error` contract to stderr. A JSON input path may be `-` when the command
accepts stdin.

The error version fixes the envelope and field meanings. Treat `code` and `details.kind` as
non-exhaustive vocabularies: branch on values you understand and retain an unknown-value fallback.

Discover the exact operation set and current wire versions:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace capabilities
```

Inspect committed sources and objects:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace inspect sources --input D:\GameProject
cargo run -p unity-asset-cli --bin unity-asset -- workspace inspect objects --input D:\GameProject
cargo run -p unity-asset-cli --bin unity-asset -- workspace inspect object --input D:\GameProject --address-json object-address.json
cargo run -p unity-asset-cli --bin unity-asset -- workspace inspect bundle-containers --input D:\GameProject --query-json container-query.json
```

Validate and canonicalize a `MutationPlan` v2 before loading a workspace:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace plan validate --plan mutation-plan.json
```

Prepare is side-effect free and emits a `PrepareReport`. Preview reparses the same plan and
inspects the prepared read-your-writes view. The report is evidence only: it cannot recreate the
single-use `PreparedChange` authority.

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace prepare --input D:\GameProject --plan mutation-plan.json
cargo run -p unity-asset-cli --bin unity-asset -- workspace preview --input D:\GameProject --plan mutation-plan.json --address-json object-address.json
```

Commit re-prepares the exact plan and publishes below an existing absolute containment root:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace commit --input D:\GameProject --plan mutation-plan.json --publication-root D:\GameProject
```

Recovery is explicit and typed. Discovery emits canonical locators; resume and abandon operate on
filesystem evidence, while finalize also reloads trusted workspace sources before attaching the
recovered revision:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- workspace recover discover --publication-root D:\GameProject
cargo run -p unity-asset-cli --bin unity-asset -- workspace recover resume --locator-json recovery-locator.json
cargo run -p unity-asset-cli --bin unity-asset -- workspace recover abandon --locator-json recovery-locator.json
cargo run -p unity-asset-cli --bin unity-asset -- workspace recover finalize --input D:\GameProject --locator-json recovery-locator.json
```

Supply one or more immutable JSON/TPK TypeTree registries before the subcommand. Earlier paths
have lookup priority:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- --typetree-registry script-types.json --typetree-registry engine.tpk workspace inspect objects --input D:\GameProject
```

## References

The CLI emits a versioned, deterministic projection whose coverage and resolution states are
bound to the loaded workspace revision:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- references graph --input D:\GameProject --unity-project --max-facts 200000
```

`ReferenceGraph` only resolves against sources present in its `WorkspaceView`; it does not discover
or open additional files while answering a query.

## Extraction

An `ExtractionRequest` v1 selects objects and representation policy. A dry run emits the canonical
`ExtractionPlan` v2 without writing artifacts:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- export --input D:\GameProject --output D:\Exports --request extraction-request.json --dry-run
```

Execute either a request or a previously captured plan. A durable manifest may be published below
the output root, and a later process can verify and resume completed artifacts:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- export --input D:\GameProject --output D:\Exports --request extraction-request.json --manifest reports/manifest.json
cargo run -p unity-asset-cli --bin unity-asset -- export --input D:\GameProject --output D:\Exports --plan extraction-plan.json --resume D:\Exports\reports\manifest.json
```

Output collision policy is `error`, `skip`, or `replace`. Failure policy is `collect-all` or
`stop-in-plan-order`. Worker, in-flight byte, open-file, output-byte, and report-byte limits are
explicit CLI options.

## Low-Level Parsers

```powershell
cargo run -p unity-asset-binary --example sniff_kind -- D:\GameProject\game.bundle
cargo run -p unity-asset-binary --example load_and_list -- D:\GameProject\game.bundle
cargo run -p unity-asset-binary --example scan_pptrs -- D:\GameProject\game.bundle -42 0
cargo run -p unity-asset-binary --example typetree_registry_json -- D:\GameProject\game.bundle
cargo run -p unity-asset-decode --example export_textures --features texture -- D:\GameProject\game.bundle D:\Exports
```

Use these APIs when building a format adapter. Application workflows should prefer
`AssetWorkspace`, because it owns identity, revision, budget, and publication invariants.

## Local Search

Start the local daemon and incrementally reconcile a Unity project:

```powershell
cargo run -p unity-asset-search-daemon -- --project-root D:\GameProject --watch
```

Index AssetBundle container paths and ignore the project root's `.gitignore`:

```powershell
cargo run -p unity-asset-search-daemon -- --project-root D:\GameProject --watch --search-everything
```

The versioned HTTP contract is `/v2`. Read endpoints are localhost-only; reindex and token
rotation require the persisted bearer token.

```powershell
cargo run -p unity-asset-search-cli -- health
cargo run -p unity-asset-search-cli -- status
cargo run -p unity-asset-search-cli -- search "type:Prefab in:Assets/UI start button" --limit 20
cargo run -p unity-asset-search-cli -- suggest "t:pr" --limit 10
cargo run -p unity-asset-search-cli -- references deadbeefdeadbeefdeadbeefdeadbeef --file-id -11500000 --limit 50
cargo run -p unity-asset-search-cli -- --token $env:UNITY_ASSET_SEARCH_TOKEN reindex --path Assets\UI\StartButton.prefab
```

The search index is a derived, consumer-owned `SearchGeneration`. Workspace commits hand off a
revision-bound, transaction-keyed `ChangeSet`; the daemon never becomes authoritative for asset
bytes.
