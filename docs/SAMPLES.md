# Samples & Golden Workflow

This repository relies on a small curated set of samples to prevent regressions while we do
fearless refactors. There are two distinct “sample lines”:

- **YAML samples**: `.prefab/.unity/.asset` text files (easy to version, great for Unity project demos).
- **Binary samples**: `AssetBundle` / `SerializedFile` bytes (required to validate UnityFS/TypeTree/ObjectHandle parity).

## Running examples

Examples are maintained per-crate.

- `cargo run -p unity-asset --example yaml_load_summary`
- `cargo run -p unity-asset --example env_load_and_list -- tests/samples`
- `cargo run -p unity-asset-binary --example sniff_kind -- tests/samples/char_118_yuki.ab`

## YAML samples (works with Unity project demos)

If you have a Unity project checkout (e.g. an official demo) with `.prefab/.unity` files:

- You can point the environment regression test at any YAML prefab via:
  - `UNITY_ASSET_YAML_PREFAB=/abs/path/to/some.prefab`
  - then run `cargo nextest run -p unity-asset environment_can_parse_external_yaml_prefab_if_provided`
- The repository also ships a minimal YAML fixture:
  - `unity-asset-yaml/tests/fixtures/MinimalGameObjectTransform.prefab`
  - This is used to ensure `Environment` resolves anchors + intra-file `fileID` references correctly.

## Binary samples (required for UnityPy-like parity)

Unity YAML alone does not exercise:

- UnityFS block layouts / archive flags
- TypeTree presence vs stripped assets
- `PPtr` externals resolution inside `SerializedFile`
- object offsets/sizes and raw bytes correctness

To close the loop, we need at least one **small binary sample** that contains:

- `GameObject` + `Transform` + `MonoBehaviour` (best-effort parsing + reference resolution)
- ideally a mix of `enableTypeTree = true/false` (to validate external registry behavior)

### Option A: Build an AssetBundle from a Unity project

1. Open the Unity project in the editor.
2. Create a prefab that contains at least `GameObject/Transform/MonoBehaviour`.
3. Add a tiny editor script to build an AssetBundle (example outline):
   - choose an output directory (outside this repo first)
   - set an AssetBundle name for the prefab
   - call `BuildPipeline.BuildAssetBundles(...)`
4. Copy the produced bundle into `tests/samples/` (only if licensing allows).

### Option B: Build a Player and extract build artifacts

Some Unity build artifacts (e.g. `globalgamemanagers`, `level0`, `sharedassets*.assets`) contain
serialized files we can parse. If you build a minimal player from the project:

1. Build for any platform.
2. Identify the produced serialized files and streamed resources (`.resS`, `.resource`).
3. Copy the smallest set into `tests/samples/` (only if licensing allows).

## Golden generation (UnityPy as oracle)

For binary samples, we maintain a UnityPy-generated golden JSON used by Rust regression tests.

- Script: `scripts/regenerate_golden_v1_unitypy.py`
- To regenerate (local-only):
  - create/activate Python venv (see your local setup)
  - run `./.venv-unitypy/bin/python scripts/regenerate_golden_v1_unitypy.py --write`

Note: golden regeneration is a local development tool and is not part of the CI release workflow.

## Contribution rules for samples

- Do not commit any `repo-ref/*` content.
- Only commit binary samples you are legally allowed to redistribute.
- Keep samples small and focused (regression value > size).
