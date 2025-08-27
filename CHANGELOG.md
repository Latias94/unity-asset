# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Nothing yet

### Changed
- Nothing yet

### Fixed
- Nothing yet

## [0.1.0] - TBD (First Release)

### Added

#### Core Features
- **YAML Processing**: Unity YAML format support with multi-document parsing
- **Binary Asset Processing**: AssetBundle and SerializedFile parsing with compression support
- **Type Safety**: Rust's type system prevents common parsing vulnerabilities
- **Async/Await API**: Optional async support for all parsing operations
- **CLI Tools**: Both synchronous and asynchronous command-line interfaces

#### Supported Formats
- **YAML Files**: .asset, .prefab, .unity, .meta files
- **Binary Assets**: AssetBundle (UnityFS, UnityWeb, UnityRaw), SerializedFile
- **Compression**: LZ4, LZMA, Brotli, Gzip support
- **Unity Versions**: 3.4 - 2023.x compatibility

#### Object Processing
- **AudioClip**: Audio processing with sample extraction
- **Texture2D**: Basic texture processing (RGBA32, RGB24, ARGB32, Alpha8)
- **Sprite**: Sprite parsing and metadata extraction
- **Mesh**: Mesh data structure parsing
- **TypeTree**: TypeTree parsing and manipulation

#### CLI Features
- **Batch Processing**: Recursive directory scanning and processing
- **Multiple Output Formats**: JSON, YAML, debug formats
- **Progress Reporting**: Real-time progress bars and statistics
- **Configurable Concurrency**: Adjustable parallel processing

### Architecture
- `unity-asset-core`: Core data structures and traits
- `unity-asset-yaml`: YAML format parsing and serialization
- `unity-asset-binary`: Binary asset parsing (AssetBundle, SerializedFile)
- `unity-asset-lib`: Main library crate
- `unity-asset-cli`: Command-line tools

### Known Limitations
- **Texture Formats**: Limited to basic uncompressed formats (RGBA32, RGB24, ARGB32, Alpha8)
- **LZMA Decompression**: Some Unity 5.x files with specific LZMA variants may fail to decompress

### Acknowledgments
This project builds upon:
- [UnityPy](https://github.com/K0lb3/UnityPy) by @K0lb3
- [unity-rs](https://github.com/yuanyan3060/unity-rs) by @yuanyan3060
