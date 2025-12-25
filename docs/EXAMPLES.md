# Examples

This repository maintains runnable examples per crate (built in CI).

## unity-asset (library)

- YAML load summary:
  - `cargo run -p unity-asset --example yaml_load_summary`
- Environment load + list:
  - `cargo run -p unity-asset --example env_load_and_list -- tests/samples`
- Bundle container lookup (UnityPy-like discovery):
  - `cargo run -p unity-asset --example env_container_lookup -- tests/samples Assets/`
- Find by `path_id` and dump JSON:
  - `cargo run -p unity-asset --example env_find_and_dump -- <path> <path_id>`
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
