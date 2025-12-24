# Contributing to Unity Asset Parser

Thank you for your interest in contributing! This document provides guidelines for contributing to the Unity Asset Parser project.

## 🚀 Quick Start

### Development Setup

```bash
# Clone the repository
git clone https://github.com/Latias94/unity-asset.git
cd unity-asset

# Build all crates
cargo build --all

# Run tests
cargo test --all

# Run with async features
cargo test --all --features async
```

### Project Structure

```
unity-asset/
├── unity-asset-core/          # Core data structures and traits
├── unity-asset-yaml/          # YAML format support  
├── unity-asset-binary/        # Binary format support
├── unity-asset-lib/           # Main library (published as "unity-asset")
├── unity-asset-cli/           # CLI tools (published as "unity-asset-cli")
├── .github/workflows/         # CI/CD pipelines
└── scripts/                   # Release and development scripts
```

## 📋 Development Workflow

### 1. Feature Development

1. **Create a feature branch**:
   ```bash
   git checkout -b feature/your-feature-name
   ```

2. **Make your changes** following our coding standards

3. **Add tests** for new functionality

4. **Run the test suite**:
   ```bash
   cargo test --all --all-features
   cargo clippy --all-targets --all-features -- -D warnings
   cargo fmt --all -- --check
   ```

5. **Update documentation** if needed

### 2. Testing

We maintain high test coverage. Please ensure:

- **Unit tests** for new functions and methods
- **Integration tests** for new features
- **UnityPy compatibility tests** for parsing functionality
- **CLI tests** for command-line features

```bash
# Run specific test suites
cargo test -p unity-asset-core
cargo test -p unity-asset-yaml
cargo test -p unity-asset-binary
cargo test -p unity-asset-cli

# Fast inner-loop (unit tests only)
cargo test -p unity-asset-binary --lib

# Slow / comprehensive suites (ignored by default)
cargo test -p unity-asset-binary --tests -- --ignored

# Test async features
cargo test --features async
```

### 3. Code Style

We use standard Rust formatting and linting:

```bash
# Format code
cargo fmt --all

# Run clippy
cargo clippy --all-targets --all-features -- -D warnings

# Check documentation
cargo doc --all-features --no-deps
```

## 🔄 Release Process

### For Maintainers

Our release process is automated through GitHub Actions:

1. **Prepare release**:
   ```powershell
   # Windows
   .\scripts\release.ps1 -Version "0.2.0" -DryRun
   .\scripts\release.ps1 -Version "0.2.0"
   ```

2. **Push to trigger CI**:
   ```bash
   git push origin main --tags
   ```

3. **GitHub Actions will**:
   - Run full test suite
   - Publish crates in dependency order
   - Create GitHub release

### Versioning Strategy

We follow [Semantic Versioning](https://semver.org/):

- **MAJOR** (1.0.0): Breaking API changes
- **MINOR** (0.1.0): New features, backward compatible
- **PATCH** (0.0.1): Bug fixes, backward compatible

### Crate Publishing Order

Due to dependencies, crates must be published in this order:

1. `unity-asset-core` (no dependencies)
2. `unity-asset-yaml` (depends on core)
3. `unity-asset-binary` (depends on core)
4. `unity-asset` (depends on all sub-crates)
5. `unity-asset-cli` (depends on main library)

## 📝 Documentation

### Code Documentation

- Use `///` for public API documentation
- Include examples in doc comments
- Document error conditions and panics
- Keep documentation up-to-date with code changes

### README Updates

When adding new features, update:
- Feature list in README.md
- Usage examples
- Performance benchmarks if applicable

## 🐛 Bug Reports

When reporting bugs, please include:

1. **Unity Asset Parser version**
2. **Rust version** (`rustc --version`)
3. **Operating system**
4. **Sample Unity file** (if possible)
5. **Steps to reproduce**
6. **Expected vs actual behavior**

## 💡 Feature Requests

For new features:

1. **Check existing issues** first
2. **Describe the use case** clearly
3. **Provide examples** of how it would be used
4. **Consider UnityPy compatibility** if applicable

## 🔒 Security

For security vulnerabilities:

1. **Do not** create public issues
2. **Email** the maintainers directly
3. **Provide** detailed reproduction steps
4. **Allow** reasonable time for fixes

## 📄 License

By contributing, you agree that your contributions will be licensed under the MIT License.

## 🙏 Recognition

Contributors will be recognized in:
- CHANGELOG.md for their contributions
- GitHub releases
- Project documentation

Thank you for helping make Unity Asset Parser better! 🚀
