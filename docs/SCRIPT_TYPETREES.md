# Script TypeTrees for MonoBehaviour Data

MonoBehaviour payloads in builds with stripped TypeTrees require a script-specific schema. The
workspace accepts immutable JSON or TPK registries so schema discovery remains outside the trusted
asset-loading path.

## Contract

Registry resolution uses:

- the SerializedFile Unity version;
- class ID `114` for MonoBehaviour;
- the 16-byte script ID from `SerializedType`, when present.

The JSON registry schema is version `2`:

```json
{
  "schema": 2,
  "entries": [
    {
      "unity_version": "2020.3.*",
      "class_id": 114,
      "script_id": "01010101010101010101010101010101",
      "type_tree": { "...": "..." }
    }
  ]
}
```

`script_id` is 32 lowercase hexadecimal characters. Unknown fields, malformed IDs, duplicate
keys, unsupported schemas, and over-budget inputs are rejected. Registry order is significant:
the first matching registry wins.

## Generate a JSON Registry

The included Python exporter can use managed assemblies or IL2CPP metadata through an installed
UnityPy and `TypeTreeGeneratorAPI` toolchain.

For a managed build:

```powershell
python scripts/export_unitypy_script_typetrees.py `
  --input "D:\Game\Game_Data\game.bundle" `
  --managed-dir "D:\Game\Game_Data\Managed" `
  --output "D:\Schemas\script-typetrees.json" `
  --verbose
```

For an IL2CPP build:

```powershell
python scripts/export_unitypy_script_typetrees.py `
  --input "D:\Game\Game_Data\game.bundle" `
  --game-root "D:\Game" `
  --output "D:\Schemas\script-typetrees.json" `
  --verbose
```

Repeat `--input` to scan several bundles or SerializedFiles. The exporter de-duplicates script IDs
in deterministic first-seen order.

The Python exporter is an optional compatibility tool, not a runtime dependency of the Rust
workspace. Its output must still pass the Rust registry parser and caller-owned budget.

## Load Registries into a Workspace

`WorkspaceOptions::with_type_tree_registry_paths` parses every JSON/TPK registry under the same
`AssetLoadBudget` used by the caller. During source loading, only required lookup keys and their
trees are copied into a frozen per-source registry. Snapshot lookup is therefore immutable and
allocation-free.

```rust
use std::path::PathBuf;

use unity_asset::AssetLoadBudget;
use unity_asset::workspace::{AssetWorkspace, WorkspaceOptions};

fn load_with_script_types() -> Result<AssetWorkspace, Box<dyn std::error::Error>> {
    let registry_paths = [
        PathBuf::from(r"D:\Schemas\script-typetrees.json"),
        PathBuf::from(r"D:\Schemas\engine.tpk"),
    ];
    let mut budget = AssetLoadBudget::default();
    let options =
        WorkspaceOptions::strict().with_type_tree_registry_paths(&registry_paths, &mut budget)?;
    let mut workspace = AssetWorkspace::with_options(options)?;
    workspace.load_path(r"D:\Game\Game_Data", &mut budget)?;
    Ok(workspace)
}
```

The CLI exposes the same policy:

```powershell
cargo run -p unity-asset-cli --bin unity-asset -- `
  --typetree-registry D:\Schemas\script-typetrees.json `
  --typetree-registry D:\Schemas\engine.tpk `
  workspace inspect objects --input D:\Game\Game_Data
```

Use `WorkspaceOptions::strict()` when missing script schemas must fail the operation.
`WorkspaceOptions::lenient()` preserves structured diagnostics and raw-object fallbacks where the
format adapter supports them.

## Low-Level Integration

Format-adapter authors may construct JSON or TPK registries from `unity-asset-binary` and attach a
registry directly to a `SerializedFile`. Every constructor still requires a caller-owned budget.
Application code should prefer `WorkspaceOptions`, because it freezes the exact schemas retained
by snapshots and prevents arbitrary callback behavior from crossing revision boundaries.

## Validation

- `workspace inspect objects` emits structured MonoBehaviour fields instead of only a raw payload.
- The object inspection reports the expected workspace ID, revision, source locator, class, and
  script metadata.
- A no-op prepare preserves the original bytes.
- A guarded mutation prepares, independently reparses, commits, and can be reopened at the new
  revision.
- A missing, ambiguous, malformed, or over-budget schema produces a structured diagnostic rather
  than a guessed layout.
