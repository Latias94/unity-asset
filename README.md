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
- ❌ **UnityPy Replacement**: UnityPy is far more mature and feature-complete
- ❌ **Production Tool**: Missing many features needed for real-world usage
- ❌ **Complete Solution**: Many Unity asset types are not supported

## 🏗️ Architecture

The project uses a workspace structure to organize different parsing capabilities:

```text
unity-asset/
├── unity-asset-core/      # Core data structures and traits
├── unity-asset-yaml/      # YAML file parsing (complete)
├── unity-asset-binary/    # Binary asset parsing (complete)
├── src/                   # CLI tools and main library
│   ├── main.rs           # Synchronous CLI tool
│   └── main_async.rs     # Asynchronous CLI tool (--features async)
└── examples/             # Usage examples and demos
```

### Current Capabilities

**🔧 YAML Processing (Basic)**
- Unity YAML format parsing for common file types (.asset, .prefab, .unity)
- Multi-document parsing support
- Basic reference resolution
- Simple filtering and querying

**🔧 Binary Asset Processing (Limited)**
- Basic AssetBundle structure parsing (UnityFS format)
- SerializedFile header reading
- TypeTree structure parsing (partial)
- Some compression support (LZ4, basic LZMA)
- Metadata extraction (basic information only)

**🔧 Object Processing (Experimental)**
- AudioClip: Basic structure parsing (limited format support)
- Texture2D: Structure parsing only (no image decoding)
- Sprite: Basic metadata extraction
- Mesh: Structure parsing (many limitations)
- TypeTree: Basic reading and structure analysis

**🔧 CLI Tools**
- Basic synchronous CLI for file inspection
- Experimental async CLI with concurrent processing
- Simple batch processing capabilities
- Basic progress reporting

**⚠️ Known Limitations**
- Many Unity asset types are not supported
- Limited object manipulation capabilities
- Incomplete compression support (some LZMA variants fail)
- No image format decoding for textures
- No audio format decoding
- Error handling needs improvement
- Performance not optimized for large files

## 🚀 Quick Start

### Installation

**Note**: This project is not yet published to crates.io. To try it out:

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
let doc = YamlDocument::load_yaml("ProjectSettings.asset", false)?;

// Get basic information
println!("Found {} entries", doc.entries().len());

// Try to find specific objects (may not work for all files)
if let Ok(settings) = doc.get(Some("PlayerSettings"), None) {
    println!("Found PlayerSettings");
}
```

### CLI Usage

```bash
# Parse a single YAML file
cargo run --bin unity-asset -- parse-yaml -i ProjectSettings.asset

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
