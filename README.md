# Unity Asset Parser

A Rust implementation of Unity asset parsing, inspired by and learning from [UnityPy](https://github.com/K0lb3/UnityPy). This project focuses on parsing Unity YAML and binary formats with Rust's memory safety and performance characteristics.

## 🎯 Project Status

**⚠️ Early Development**: This is a learning project and reference implementation. It is **not production-ready** and has significant limitations compared to mature tools like UnityPy.

### What This Project Is
- 📚 **Learning Exercise**: Understanding Unity's file formats through Rust implementation
- 🔍 **Parser Focus**: Emphasis on parsing and data extraction rather than manipulation
- 🦀 **Rust Exploration**: Exploring how Rust's type system can help with binary parsing
- 📖 **Reference Implementation**: Code that others can learn from and build upon

### What This Project Is NOT
- ❌ **UnityPy Replacement**: UnityPy remains the most mature Python solution
- ❌ **Asset Editor**: This is a read-only parser, not an asset creation/editing tool

## 🏗️ Architecture

The project uses a workspace structure to organize different parsing capabilities:

```text
unity-asset/
├── unity-asset-core/      # Core data structures and traits
├── unity-asset-yaml/      # YAML file parsing (stable)
├── unity-asset-binary/    # Binary asset parsing (advanced, WIP)
├── unity-asset-lib/       # Main library crate (published as `unity-asset`)
├── unity-asset-cli/       # CLI tools
│   ├── main.rs           # Synchronous CLI tool
│   └── main_async.rs     # Asynchronous CLI tool (--features async)
├── examples/             # Usage examples and demos
└── tests/                # Integration tests and sample files
```

### Current Capabilities

#### 🔧 YAML Processing (Complete)
- Unity YAML format parsing for common file types (.asset, .prefab, .unity)
- Multi-document parsing support
- Reference resolution and cross-document linking
- Filtering and querying capabilities
- Serialization back to YAML format

#### 🔧 Binary Asset Processing (Advanced, WIP)
- AssetBundle structure parsing (UnityFS format)
- SerializedFile parsing with full object extraction
- TypeTree structure parsing and dynamic object reading
- Compression support (LZ4, LZMA, Brotli)
- Metadata extraction and analysis (experimental; includes dependency graph, best-effort hierarchy/component mapping, and external reference resolution via `externals`)
- Performance monitoring and basic statistics

#### 🔧 Object Processing (Partial)
- **AudioClip**: Full format support (Vorbis, MP3, WAV, AAC) via `unity-asset-decode` (Symphonia-based decoder)
- **Texture2D**: Complete parsing + best-effort decoding + PNG export via `unity-asset-decode`
- **Sprite**: Full metadata extraction + atlas support + image cutting via `unity-asset-decode`
- **Mesh**: Structure parsing + vertex data extraction + basic export via `unity-asset-decode`
- **GameObject/Transform**: Basic TypeTree-based hierarchy & component mapping (best-effort; still WIP)

#### 🔧 CLI Tools (Usable, WIP)
- Synchronous CLI for file inspection and batch processing
- Asynchronous CLI with concurrent processing and progress tracking
- Export capabilities (PNG, OGG, WAV, basic mesh formats)
- Comprehensive metadata analysis and reporting
- Basic progress reporting

**⚠️ Known Limitations**
- Some advanced Unity asset types not yet implemented (MonoBehaviour scripts, complex shaders)
- Object manipulation is read-only (no writing back to Unity formats)
- Some edge cases in LZMA decompression may fail on corrupted data
- Advanced texture formats require `unity-asset-decode` `texture-advanced` feature (DXT, ETC, ASTC)
- Audio decoding requires `unity-asset-decode` `audio` feature (Symphonia integration)
- Large file performance could be optimized further
- Error messages could be more user-friendly
- Some metadata/dependency/hierarchy analyses are currently simplified placeholders
- Object data is lazily accessed by default; use `SerializedFileParser::from_bytes_with_options(data, true)` if you explicitly need per-object preloaded buffers
- For large objects without TypeTree, raw bytes are not expanded into `_raw_data` for performance; use `UnityObject::raw_data()`

## 🚀 Quick Start

### Installation

**Note**: This project will be published to crates.io soon. For now, to try it out:

```bash
# Clone and build from source
git clone https://github.com/Latias94/unity-asset.git
cd unity-asset

# Build the library
cargo build --all

# Try the CLI tools
cargo run --bin unity-asset -- --help
cargo run --features async --bin unity-asset-async -- --help
```

Once published, you'll be able to install it with:

```toml
# Add to your Cargo.toml
[dependencies]
unity-asset = "0.1.0"
```

```bash
# Install CLI tools
cargo install unity-asset-cli
```

### Testing Status

We have basic tests for core functionality, but this is not a comprehensive test suite. Some tests pass, others reveal limitations in our implementation.

### Comparison with UnityPy

[UnityPy](https://github.com/K0lb3/UnityPy) is a mature, feature-complete Python library for Unity asset manipulation. This Rust project is:

- **Much less mature**: UnityPy has years of development and community contributions
- **More limited**: We focus on parsing, not manipulation or export
- **Learning-oriented**: This project helps understand Unity formats through Rust
- **Experimental**: Many features are incomplete or missing

If you need a production tool for Unity asset processing, **use UnityPy instead**.

## 📝 Basic Usage Examples

### YAML File Parsing

```rust
use unity_asset::{YamlDocument, UnityDocument};

// Load a Unity YAML file
let (doc, warnings) = YamlDocument::load_yaml_with_warnings("ProjectSettings.asset", false)?;
for w in warnings {
    eprintln!("warning: {}", w);
}

// Get basic information
println!("Found {} entries", doc.entries().len());

// Try to find specific objects (may not work for all files)
if let Ok(settings) = doc.get(Some("PlayerSettings"), None) {
    println!("Found PlayerSettings");
}
```

### UnityPy-like Environment (YAML + Binary)

```rust
use unity_asset::environment::{Environment, EnvironmentObjectRef};
use unity_asset_binary::typetree::JsonTypeTreeRegistry;
use std::sync::Arc;

let mut env = Environment::new();

// Optional: provide an external TypeTree registry for stripped assets (best-effort).
// This can improve coverage when `enableTypeTree = false` in serialized files.
// let registry = JsonTypeTreeRegistry::from_path("typetree_registry.json")?;
// env.set_type_tree_registry(Some(Arc::new(registry)));

env.load("tests/samples")?;

// `path_id` is only unique within a single SerializedFile.
// Use `BinaryObjectKey` when you need a globally-unique handle you can round-trip later.
let sources = env.binary_sources();
if let Some((_kind, source_path)) = sources.first() {
    let keys = env.find_binary_object_keys_in_source(source_path, 1);
    if let Some(key) = keys.first() {
        let _parsed = env.read_binary_object_key(key)?;
    }
}

// Unity `PPtr` resolution needs a context object because `fileID` indexes into the context file's externals.
// For the common case `fileID=0`, it points to the same SerializedFile as the context.
if let Some(obj_ref) = env.find_binary_object(1) {
    let _pptr_obj = env.read_binary_pptr(&obj_ref, 0, 1)?;
}

// AssetBundles often expose a container mapping from asset paths to objects.
// This is the primary discovery mechanism in UnityPy.
// Note: This is best-effort; when TypeTree is stripped, we fall back to a raw binary parser for `m_Container`.
let container = env.find_binary_object_keys_in_bundle_container("Assets/");
for (asset_path, key) in container.into_iter().take(10) {
    let _obj = env.read_binary_object_key(&key)?;
    println!("{} -> path_id={}", asset_path, key.path_id);
}

for obj in env.objects() {
    match obj {
        EnvironmentObjectRef::Yaml(class) => {
            let _ = &class.class_name;
        }
        EnvironmentObjectRef::Binary(obj_ref) => {
            // Parse on-demand (best-effort)
            let _parsed = obj_ref.read()?;
            let _key = obj_ref.key();
        }
    }
}
```

### CLI Usage

```bash
# Parse a single YAML file
cargo run --bin unity-asset -- parse-yaml -i ProjectSettings.asset

# List bundle nodes (files) for debugging/inspection
cargo run --bin unity-asset -- list-bundle -i tests/samples/char_118_yuki.ab --filter "CAB-" --verbose

# Find objects via AssetBundle `m_Container` (discovery)
cargo run --bin unity-asset -- find-object -i tests/samples/char_118_yuki.ab --pattern "Assets/" --limit 20 --verbose

# Filter by type (useful for scripts/batch workflows)
cargo run --bin unity-asset -- find-object -i tests/samples/char_118_yuki.ab --class-name "Texture2D" --limit 20

# Filter by object name (best-effort; requires TypeTree and a name field)
cargo run --bin unity-asset -- find-object -i tests/samples/char_118_yuki.ab --name "yuki" --limit 20 --verbose

# Dump an external TypeTree registry (best-effort fallback for stripped assets)
cargo run --bin unity-asset -- dump-typetree-registry -i tests/samples -o typetree_registry.json --version-prefix

# Use an external TypeTree registry during discovery/inspection (best-effort)
cargo run --bin unity-asset -- --typetree-registry typetree_registry.json find-object -i tests/samples --pattern "Assets/" --limit 20 --verbose

# `--typetree-registry` also accepts a UnityPy-compatible `.tpk` file.

# Inspect a single object (TypeTree / Null-field debugging)
# - Easiest: copy/paste the `key=bok2|...` line from `find-object --verbose` and use `--key`.
# - Or pass the location fields explicitly (use `--kind serialized` for standalone `.assets` files).
cargo run --bin unity-asset -- inspect-object -i tests/samples --key 'bok2|bundle|0|1|<outer_len>|tests/samples/char_118_yuki.ab|0|' \
    --max-depth 6 --max-items 200 --filter "m_StreamData"
# If you suspect TypeTree mismatches, enable fail-fast parsing and print warnings:
# `--strict` (fail-fast) and `--show-warnings` (print TypeTree warnings)

# Scan PPtr references without fully parsing objects (fast dependency/graph workflows)
cargo run --bin unity-asset -- scan-pptr -i tests/samples/char_118_yuki.ab --kind bundle --asset-index 0 --limit 5
cargo run --bin unity-asset -- scan-pptr -i tests/samples/char_118_yuki.ab --kind bundle --asset-index 0 --class-id 114 --json

# Export objects from AssetBundles via `m_Container` (UnityPy-like workflow)
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --limit 50
# Decode known types (best-effort):
# - `AudioClip`: export embedded/streamed audio bytes (e.g. `.ogg`) or decode to `.wav` fallback
# - `Texture2D`: decode and export as `.png`
# - `Sprite`: resolve referenced `Texture2D`, crop sprite rect, and export as `.sprite.png`
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --decode --limit 50
#
# Parallelize export/decode work:
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --decode --jobs 0
#
# Filter by type (reduces work on large bundles):
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --class-name "Texture2D" --decode --jobs 0
#
# Re-runs: keep output deterministic and resumable via a manifest
# - `--continue-on-error` records failures as `status=failed` with an error string
# - `--resume` skips already-exported entries (when the previous output path still exists)
# - `--retry-failed-from` re-exports only entries that failed previously
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --decode \\
    --manifest out/manifest.json --continue-on-error --jobs 0
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --decode \\
    --retry-failed-from out/manifest.json --manifest out/manifest.retry.json --continue-on-error --jobs 0
cargo run --bin unity-asset -- export-bundle -i tests/samples -o out/ --pattern "Assets/" --decode \\
    --resume out/manifest.retry.json --manifest out/manifest.resume.json --jobs 0
#
# Note: outputs are raw SerializedFile object bytes (`.bin`), not necessarily the original file format.
#
# Note (streamed resources): some `AudioClip`/`Texture2D` objects reference external `.resS`/`.resource`
# files that are not embedded inside the bundle. `export-bundle --decode` will try:
# 1) resource nodes inside the same bundle (UnityFS)
# 2) sibling resource files on disk (same directory / `StreamingAssets/`)
# If the resource file is missing, it falls back to exporting raw `.bin`.

# Try async processing (experimental)
cargo run --features async --bin unity-asset-async -- \
    parse-yaml -i Assets/ --recursive --progress
```

## 🏗️ Architecture Details

This project is organized as a Rust workspace with separate crates for different concerns:

- **`unity-asset-core`**: Core data structures and traits
- **`unity-asset-yaml`**: YAML format parsing
- **`unity-asset-binary`**: Binary format parsing (AssetBundle, SerializedFile)
- **`unity-asset-lib`**: Main library crate (published as `unity-asset`)
- **`unity-asset-cli`**: Command-line tools (published as `unity-asset-cli`)

## 🙏 Acknowledgments

This project is a learning exercise inspired by and learning from several excellent projects:

### **[UnityPy](https://github.com/K0lb3/UnityPy)** by [@K0lb3](https://github.com/K0lb3)
- The gold standard for Unity asset manipulation
- Our primary reference for understanding Unity formats
- Test cases and expected behavior patterns

### **[unity-rs](https://github.com/yuanyan3060/unity-rs)** by [@yuanyan3060](https://github.com/yuanyan3060)
- Pioneering Rust implementation of Unity asset parsing
- Architecture and parsing technique inspiration
- Binary format handling examples

### **[unity-yaml-parser](https://github.com/socialpoint-labs/unity-yaml-parser)** by [@socialpoint-labs](https://github.com/socialpoint-labs)
- Original inspiration for this project
- YAML format expertise and reference resolution patterns
- Clean API design principles

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.
