# Migrating to Asset Workspace

This release deliberately removes the mutable compatibility facade and legacy export commands.
There are no compatibility aliases. Migrate to the revision-bound Workspace contracts rather than
wrapping the removed APIs.

This is the only non-historical document that names removed public symbols and commands.

`ExtractionPlan`, `ExtractionManifest`, and `ExtractionReport` are now version 2. Version 1 plans
persisted an ignored Sprite Unity version, while version 1 manifests and reports did not declare
the complete diagnostic vocabulary. Re-plan the request and create fresh resume evidence.

`ExtractionExecutionLimits::new` now rejects `max_open_files` values below
`ExtractionExecutionLimits::MIN_OPEN_FILES` (currently 5). The minimum covers the run lock,
staging file, parent-directory handles, and digest verification required by safe publication.

`ExtractionExecutionError`, `ExtractionModelError`, and `ExtractionDiagnosticCode` are now
non-exhaustive. Downstream matches must retain a wildcard branch so future diagnostic additions do
not require another source-breaking release.

## Migration Summary

| Removed surface | Replacement |
| --- | --- |
| `Environment` | `AssetWorkspace` for ownership and mutation; `WorkspaceSnapshot` for immutable reads |
| `BinarySource` / `BinarySourceKind` | `SourceOpenRequest`, `SourceAlias`, `SourceKind`, `SourceLocator`, and `SourceId` |
| `BinaryObjectKey` | Persisted `ObjectAddress`; in-process `RevisionedObjectHandle` |
| `EnvironmentObjectRef` | `WorkspaceObjectInspection`, `WorkspaceObject`, and `WorkspaceObjectValue` |
| `EnvironmentReporter` / `EnvironmentWarning` | Structured diagnostics and typed reports; formatting belongs to the application |
| `set_type_tree_registry*` | `WorkspaceOptions::with_type_tree_registry_paths` before workspace creation |
| `ScriptTypeTreeGenerator` callback | Immutable, budgeted JSON or TPK registry loaded through `WorkspaceOptions` |
| `UnityClassRegistry` | Direct immutable class values and schema provenance; no constructor registry |
| `PythonLikeUnityDocument` / `PythonLikeUnityClass` | Typed `YamlDocument`, `UnityClass`, `UnityValue`, and Workspace inspection |
| `DynamicAccess` / `DynamicValue` | `UnityClass` and `UnityValue` typed accessors |
| `UnityAssetError::TypeTreeShape` | `TypeTreeWriteError::Shape`; object mutations report `SerializedObjectEncodeError::ReplacementShape` |
| `SerializedObjectEncodeError::{ReplacementValue, Rewrite}` with `UnityAssetError` sources | The same variants with `TypeTreeWriteError` sources |
| `unity_asset_write::Endian` | `unity_asset_write::ByteOrder`, re-exported from `unity-asset-binary` |
| Direct mutable class/document and `save*` APIs | `MutationPlan`, schema recipes, `prepare`, preview, and `commit` |
| Legacy dependency/session graph types | `ReferenceGraph` |
| Legacy export manifest and export sessions | `ExtractionRequest`, `ExtractionPlan`, `ExtractionManifest`, and `ExtractionReport` |
| `ExtractionExecutor::execute_with_manifest` and separate resume/manifest arguments | Build one `ExtractionRunOptions` value with `with_resume` and/or `with_manifest_path`, then call `ExtractionExecutor::execute` |
| `TextureExporter::export_*`, `export_auto`, `export_validated`, and texture `ExportOptions` | Open and buffer the destination in the application, then call the explicit `TextureExporter::write_png`, `write_jpeg`, `write_bmp`, or `write_tiff` encoder |
| `AudioExporter::export_*`, `export_auto`, `export_validated`, `AudioFormat`, and audio `ExportOptions` | Open and buffer the destination in the application, then call `AudioExporter::write_wav`, `write_raw_pcm`, or `write_standard_source` |
| `TextureExporter::supported_formats` / `is_format_supported` and `AudioExporter::supported_formats` / `is_format_supported` | The concrete `write_*` methods are the encoder capability surface; decoder support remains available from `TextureProcessor` and `AudioProcessor` |
| `TextureExporter::create_filename` / `AudioExporter::create_filename` | Output naming belongs to the application, or to `ExtractionPlanner` when using the extraction pipeline |
| `TextureExporter::validate_for_export` / `AudioExporter::validate_for_export` | Call the selected `write_*` method; each writer validates the exact dimensions, frame shape, container, and codec constraints it requires before publishing bytes |
| `texture::export_image` / `audio::export_audio` | Call the corresponding explicit `TextureExporter::write_*` or `AudioExporter::write_*` encoder |
| `TextureProcessor::process_and_export` / `AudioProcessor::process_and_export` | `process_and_write_png` / `process_and_write_wav` with a caller-owned writer |
| `SpriteProcessor::extract_sprite_image` and `process_sprite_with_texture` | `render_sprite` for an `RgbaImage`, or `write_sprite_png` for a caller-owned writer |
| `SpriteResult`, `SpriteParser`, and `SpriteProcessor::parse_sprite` | Use strict `SpriteLayout::inspect` for extraction metadata; the unversioned raw fallback parser was removed |
| `SpriteManager`, `SpriteConfig`, `SpriteAtlas`/`SpriteInfo`, `SpriteStats`, `create_*_manager`, `ProcessingOptions`, and Sprite feature/validation/statistics helpers | Use `SpriteLayout` for inspection or `SpriteProcessor` for rendering caller-owned `Sprite` data; the library no longer advertises unimplemented atlas, transform, mesh, physics, caching, or parallel-processing capabilities |
| `SpriteProcessor::new(version)` | Call `new()` without a version; the prior parameter was ignored and did not provide version-aware parsing |

## Loading Sources

### Before

```rust,ignore
let mut env = Environment::new();
env.load(path_or_directory, &mut budget)?;
```

### After

```rust,no_run
use unity_asset::{AssetLoadBudget, SourceAlias, SourceKind};
use unity_asset::workspace::{AssetWorkspace, SourceOpenRequest};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut budget = AssetLoadBudget::default();
    let mut workspace = AssetWorkspace::new()?;

    let request = SourceOpenRequest::new(
        "Build/game.bundle",
        SourceAlias::new("Build/game.bundle")?,
    )
    .with_kind_hint(SourceKind::AssetBundle);
    workspace.load_source(request, &mut budget)?;
    Ok(())
}
```

The Rust library no longer owns an implicit directory walk. Enumerate caller-trusted files,
apply the application's ignore policy, assign stable aliases, and call `load_source` for each
selected root. `load_path` is a convenience for one explicit file. The CLI still accepts a file
or directory and applies its own budgeted discovery policy.

For external TypeTrees, configure the workspace before loading:

```rust,no_run
use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{AssetWorkspace, WorkspaceOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut budget = AssetLoadBudget::default();
    let options = WorkspaceOptions::lenient()
        .with_type_tree_registry_paths(&["registry.json", "unity.tpk"], &mut budget)?;
    let _workspace = AssetWorkspace::with_options(options)?;
    Ok(())
}
```

Registry order is deterministic: earlier paths take precedence. Runtime generator callbacks are
not retained by snapshots.

## Low-Level TypeTree Writes

Canonical TypeTree validation, encoding, and byte-preserving rewrite now belong to the compiled
`unity_asset_binary::typetree::TypeTreeSchema`. This is a breaking boundary change in the
unreleased workspace release. Callers that inspect object-encoding errors must update their source
matches from `UnityAssetError` to `TypeTreeWriteError`.

`TypeTreeWriteError::Budget` is promoted to `SerializedObjectEncodeError::Budget`. A
`TypeTreeWriteError::Shape` raised while applying a mutation becomes
`SerializedObjectEncodeError::ReplacementShape`; other value-validation failures remain under
`ReplacementValue`, and template failures remain under `Rewrite`.

## Source and Object Identity

`BinaryObjectKey` combined a physical path, source kind, optional asset index, and path ID. That
shape admitted invalid combinations and was not revision-bound.

Use the following split:

- `SourceLocator`: portable logical route from a root alias through nested container members;
- `ObjectAddress`: portable object identity containing a source locator and format-local identity;
- `SourceId` / `ObjectId`: opaque identity inside one workspace;
- `RevisionedObjectHandle`: in-process capability bound to one workspace revision.

Do not persist `SourceId`, `ObjectId`, a handle, or `Display` output. Persist the versioned
`ObjectAddress` JSON and resolve it against the intended snapshot:

```rust,no_run
use unity_asset::{AssetLoadBudget, ObjectAddress};
use unity_asset::workspace::{WorkspaceLookup, WorkspaceView};

fn resolve(
    view: &impl WorkspaceView,
    address: &ObjectAddress,
    budget: &mut AssetLoadBudget,
) -> Result<(), Box<dyn std::error::Error>> {
    match view.resolve_object(address, budget)? {
        WorkspaceLookup::Resolved(handle) => {
            let object = view.read_object(&handle, budget)?;
            println!("{}", object.class().class_name());
        }
        WorkspaceLookup::Unloaded => eprintln!("source is known but not loaded"),
        WorkspaceLookup::Missing => eprintln!("object is missing"),
        WorkspaceLookup::Ambiguous { candidates } => {
            eprintln!("{} candidates", candidates.len());
        }
        WorkspaceLookup::Invalid { diagnostic } => {
            eprintln!("{}", diagnostic.message());
        }
    }
    Ok(())
}
```

## Inspection

Replace direct access to `Environment::binary_assets`, `Environment::bundles`,
`Environment::yaml_documents`, `Environment::objects`, and related aggregate maps with
`WorkspaceInspector`.

```rust,no_run
use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{AssetWorkspace, WorkspaceInspector};

fn inspect(workspace: &AssetWorkspace) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = workspace.snapshot();
    let mut budget = AssetLoadBudget::default();
    let inspector = WorkspaceInspector::new(&snapshot);

    for source in inspector.sources(&mut budget)? {
        println!("{:?}", source.format());
    }
    for object in inspector.objects(&mut budget)? {
        println!("{:?}", object.address());
    }
    Ok(())
}
```

Inspection values are owned, versioned, serialize-only projections. They do not expose mutable
parser maps or require reparsing source bytes.

### Bundle container discovery

Replace `bundle_container_entries`, `find_bundle_container_entries`,
`find_binary_object_keys_in_bundle_container`, and local `m_Container` fallback parsers with:

1. one `ReferenceGraph`;
2. `ExtractionPlanner::bundle_container_occurrences`;
3. a versioned `BundleContainerQuery`.

The result preserves source order, owner address, field path, raw `{fileID, pathID}`, structured
resolution, and diagnostics. It does not discard unresolved occurrences.

## References

Replace `read_binary_pptr`, `scan_pptr`, ad hoc dependency maps, and the old graph/session
interfaces with one `ReferenceGraph`:

```rust,no_run
use unity_asset::AssetLoadBudget;
use unity_asset::reference::ReferenceGraphBuildOptions;
use unity_asset::workspace::AssetWorkspace;

fn graph(workspace: &AssetWorkspace) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = workspace.snapshot();
    let graph = snapshot.reference_graph(
        ReferenceGraphBuildOptions::unbounded(),
        &mut AssetLoadBudget::default(),
    )?;
    println!("{} facts", graph.facts().len());
    Ok(())
}
```

The graph owns incoming, outgoing, roots, leaves, closure, cycle, and deterministic projection
behavior. Build a graph from `PreparedView` to query staged references before commit.

## Mutation

Direct `UnityClass::set`, `properties_mut`, `value_at_path_mut`, mutable document entry access,
and `YamlDocument::save*` bypassed source expectations and container rewrites. The replacement is:

```text
snapshot
  -> schema inspection or recipe lowering
  -> canonical MutationPlan
  -> AssetWorkspace::prepare
  -> PreparedView inspection
  -> AssetWorkspace::commit
  -> CommitReport / RecoveryLocator / ChangeSet
```

Use `MutationPlanBuilder` to combine validated generic operations or use
`SchemaRecipePlanner` for domain operations. Every plan is bound to:

- `WorkspaceId`;
- base `WorkspaceRevision`;
- expected source fingerprints;
- continuous operation ordinals;
- schema/value guards;
- content-addressed payloads.

`PreparedChange` is deliberately not serializable, cloneable, or reconstructible from
`PrepareReport`. In a Rust process, keep it alive and pass it directly to `commit`. Across a
process boundary, persist the canonical plan and re-run prepare.

## Commit and Recovery

Create an existing absolute `PublicationTarget`, then consume the prepared authority:

```rust,no_run
use std::path::Path;

use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{
    AssetWorkspace, CommitReport, MutationPlan, PrepareOptions, PublicationTarget,
};

fn commit(
    workspace: &mut AssetWorkspace,
    plan: MutationPlan,
    root: &Path,
) -> Result<CommitReport, Box<dyn std::error::Error>> {
    let mut budget = AssetLoadBudget::default();
    let prepared = workspace.prepare(plan, PrepareOptions::default(), &mut budget)?;
    let target = PublicationTarget::in_place(root)?;
    Ok(workspace.commit(prepared, target, &mut budget)?)
}
```

The reported atomicity is `PerArtifactRecoverable`: each replacement is atomic, while the
multi-artifact set is made recoverable by its journal. It is not a cross-file atomic visibility
claim.

After interruption:

1. call `PublicationTarget::discover_recoveries`;
2. pass a selected `RecoveryLocator` to `AssetWorkspace::recover_at` or
   `AssetWorkspace::abandon_at`;
3. if requested by the outcome, recreate the workspace with its persisted `WorkspaceId`, reopen
   caller-trusted sources, and call `finalize_recovery_at`.

Never discover source paths by parsing journal text.

## CLI Migration

The CLI now has typed command families rather than one command per internal parser path.

| Removed command | Current command |
| --- | --- |
| `find-object` | `workspace inspect objects`, or `workspace inspect bundle-containers` for asset-path selection |
| `inspect-object` | `workspace inspect object --address-json ...` |
| `list-objects` | `workspace inspect objects` |
| `scan-pptr` | `references graph` |
| `deps` | `references graph` |
| `project-graph` | `references graph --unity-project` |
| `stats` / `stats-pathid` | `workspace inspect sources` |
| `extract` | `export` |
| `export-bundle` | `export --request ...` or `export --plan ...` |
| `export-serialized` | `export --request ...` or `export --plan ...` |
| `dump-typetree-registry` | No dump replacement; supply an immutable JSON/TPK registry with `--typetree-registry` |
| Separate async command binary | Use the same typed commands; async is an implementation feature, not a second protocol |

Start every automation migration with:

```bash
unity-asset workspace capabilities
```

### Object inspection

```bash
unity-asset workspace inspect objects --input project > objects.json
unity-asset workspace inspect object \
  --input project \
  --address-json object-address.json
```

Take the structured `address` field from an object inspection result. Do not parse a label or
copy path ID fields into a new ad hoc key.

### Prepare and preview

```bash
unity-asset workspace plan validate --plan mutation-plan.json
unity-asset workspace prepare --input project --plan mutation-plan.json
unity-asset workspace preview \
  --input project \
  --plan mutation-plan.json \
  --address-json object-address.json
```

The CLI prepare command emits only `PrepareReport`. Preview re-prepares and reads the staged view.
Only one structured input may use stdin in a command.

### Commit and recovery

```bash
unity-asset workspace commit \
  --input project \
  --plan mutation-plan.json \
  --publication-root /absolute/output-root

unity-asset workspace recover discover \
  --publication-root /absolute/output-root
unity-asset workspace recover resume \
  --locator-json recovery-locator.json
unity-asset workspace recover abandon \
  --locator-json recovery-locator.json
unity-asset workspace recover finalize \
  --input project \
  --locator-json recovery-locator.json
```

CLI commit re-prepares from the canonical plan and then commits in the same process. It does not
accept a serialized prepared session.

### Extraction

Legacy export request JSON and manifests are not accepted. Build a current
`ExtractionRequest`, use `--dry-run` to obtain its canonical `ExtractionPlan`, then execute that
plan:

```bash
unity-asset export \
  --input project \
  --output out \
  --request extraction-request.json \
  --dry-run > extraction-plan.json

unity-asset export \
  --input project \
  --output out \
  --plan extraction-plan.json \
  --manifest manifest.json
```

The current manifest records normalized intent, workspace revision, source and plan identities,
relative artifact paths, statuses, diagnostics, and content digests. It does not use the legacy
session schema.

## Search Handoff

Commit returns a transaction- and revision-bound `ChangeSet`. An authoritative in-process consumer
passes that value and its matching `WorkspaceView` to `SearchIndex::reindex_workspace`. Do not call
the indexer from inside the workspace transaction and do not roll back committed assets when
derived indexing fails.

The search consumer is idempotent by transaction identity and publishes a new complete generation
through its generation barrier. The filesystem daemon does not accept a bare `ChangeSet`, because
it cannot reconstruct the caller's historical `WorkspaceView` after files advance again. Startup,
watcher, and periodic reconciliation repair missed cross-process delivery.

### Search 0.4 contract migration

The current search contract intentionally replaces the 0.3 `/v1` transport and the unreleased
`/v2` and `/v3` HTTP development contracts with project-bound local IPC. There is no compatibility
listener or route. Update the daemon and CLI together; old clients cannot connect, while a new
client with an unsupported business revision receives a bootstrap incompatibility result before
business DTOs are parsed.

Rust callers must make these source changes:

| Previous surface | 0.4 replacement |
|---|---|
| Released 0.3 unversioned, flat index status | versioned protocol `StatusResponse` with revision and generation evidence |
| Pre-release `/v2` `StatusResponse.progress` / `IndexProgress` | `indexing`, `GenerationStatus::building_revision`, and the active generation stamp |
| Pre-release coordinator executor returning `ReindexReceipt` | return `ReindexExecution::new(receipt, terminal_status)` |
| Released 0.3 `--no-auto-reindex` | `--no-startup-reindex` |
| Released 0.3 `--watch-reconcile-interval-ms` | `--reconcile-interval-ms` |
| HTTP URL, port, and bearer-token options | explicit project root plus verified endpoint discovery |
| synchronous `POST /reindex` completion | reindex admit, status, wait, and bounded cancel operations |

Periodic reconciliation now defaults to five minutes and runs independently of `--watch`. Set
`--reconcile-interval-ms 0` only when another process owns reconciliation.

The released 0.3 index used `tantivy-v2`, `refs-tantivy-v1`, and `state-v2.json`; 0.4 replaces that
layout with immutable generations and durable generation heads. Stop the old daemon and delete its
derived index root before the first 0.4 start. Remove any obsolete `token` or `daemon.token` file;
the replacement daemon uses project-bound local IPC with operating-system peer verification and
does not create bearer credentials. It rebuilds all projections from authoritative project files.

Generation-head v2 is a one-way authority for 0.4 derived storage. Pre-release generation-head v1
development indices can be opened and upgraded, but this is not a compatibility path for the
released 0.3 layout. Before downgrading after 0.4 has written a head, delete the derived search index
and let the target binary rebuild it. Asset sources and workspace publication journals are
unaffected.

## No Compatibility Layer

Do not add aliases for `Environment`, reconstruct `BinaryObjectKey`, reintroduce mutable public
maps, or translate new typed errors into old display strings. Those approaches preserve the
invalid states this migration removes.

At the boundary of an older application:

1. translate trusted configuration into `SourceOpenRequest`;
2. persist `SourceLocator` and `ObjectAddress`, not runtime IDs;
3. replace display parsing with serialized fields;
4. move writes into canonical plans and the prepare/commit lifecycle;
5. delete the translation boundary after all callers use the new contracts.
