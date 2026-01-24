# Script TypeTrees (MonoBehaviour) Workflow

UnityPy can generate MonoBehaviour TypeTrees dynamically using `TypeTreeGeneratorAPI` by inspecting game assemblies (managed `.dll` or IL2CPP metadata).

This Rust rewrite intentionally keeps that mechanism **pluggable**:

- At runtime, the parser can resolve script-specific TypeTrees via a `script_id` lookup (Hash128 from `SerializedType`).
- You can feed those TypeTrees from an **external workflow** (recommended for now), or implement a native generator later.

This document describes the external workflow that produces a JSON registry (`schema: 2`) consumable by this repository.

## Why this exists

Modern games often ship **stripped TypeTrees**. For MonoBehaviours, that means you can usually read the base header, but you cannot reliably parse or write the script-defined fields without a script-specific TypeTree.

UnityPy solves this using `Environment.typetree_generator` (backed by `TypeTreeGeneratorAPI`). This repo mirrors the capability via a `script_id`-keyed registry hook.

## Output format (JSON registry schema 2)

The exporter writes a JSON file like:

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

Notes:
- `script_id` is a 16-byte `Hash128`, encoded as 32 lowercase hex chars.
- Extra fields may be present (e.g. `assembly`, `fullname`) and are ignored by the loader.

## Prerequisites

- Python 3.10+ recommended
- A Python environment that can import UnityPy dependencies
- `TypeTreeGeneratorAPI` installed in that environment
- The repo’s vendored UnityPy snapshot (already present at `repo-ref/UnityPy`)

## Export script TypeTrees from UnityPy

Script: `scripts/export_unitypy_script_typetrees.py`

### Managed build (Mono / IL2CPP disabled)

Provide the `Managed` directory containing your game’s `.dll` files:

```powershell
python scripts/export_unitypy_script_typetrees.py `
  --input "D:\path\to\some.bundle" `
  --managed-dir "D:\path\to\Game_Data\Managed" `
  --output "D:\tmp\script-typetrees.json" `
  --verbose
```

### IL2CPP build

Provide the game root (the folder that contains `GameAssembly.dll` and `*_Data/`):

```powershell
python scripts/export_unitypy_script_typetrees.py `
  --input "D:\path\to\some.bundle" `
  --game-root "D:\path\to\Game" `
  --output "D:\tmp\script-typetrees.json" `
  --verbose
```

### Multiple inputs

You can repeat `--input` to scan multiple bundles / `.assets` files. The exporter de-duplicates by `script_id` (first seen wins).

## Load the registry in Rust

The simplest is to load the JSON registry into the `Environment` so `ObjectHandle` can resolve stripped TypeTrees:

```rust
use unity_asset::Environment;

let mut env = Environment::new();
env.set_type_tree_registry_from_paths(&["D:\\tmp\\script-typetrees.json"])?;
env.load("D:\\path\\to\\bundles")?;
```

If you only need the lower-level loader, use `unity_asset_binary::typetree::JsonTypeTreeRegistry` and attach it to a `SerializedFile` via `set_type_tree_registry(...)`.

## Validation checklist

- A MonoBehaviour that previously parsed as raw bytes (`_raw_data_len` present) now parses as structured fields.
- After edits, the saved asset/bundle can be loaded by UnityPy and the changes are observable.

