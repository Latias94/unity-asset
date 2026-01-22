# Release Process

This project uses [release-plz](https://release-plz.dev/) to automate the release process, with separate CI workflows for development and release.

## 🔄 Workflow Overview

### CI Workflow (`.github/workflows/ci.yml`)
**Purpose**: Development quality assurance
**Triggers**: Push to `main`/`develop`, Pull Requests to `main`
**Features**:
- Multi-feature matrix testing (`""` and `"async"`)
- Code formatting checks (`cargo fmt`)
- Comprehensive clippy linting
- CLI tools functionality testing
- Security auditing (`cargo audit`)
- Documentation building
- Dependency caching for faster builds

### Release Workflow (`.github/workflows/release.yml`)
**Purpose**: Automated publishing
**Triggers**: Push to `main` branch only
**Features**:
- Automatic Release PR creation/updates
- Automated publishing to crates.io
- GitHub releases with changelogs
- Git tagging

## 🚀 How it works

1. **Development**: CI workflow runs on every push and PR, ensuring code quality
2. **Automatic Release PRs**: When you push commits to `main`, release-plz automatically:
   - Analyzes your commits using [Conventional Commits](https://www.conventionalcommits.org/)
   - Determines the next version based on semantic versioning
   - Updates `Cargo.toml` versions and `CHANGELOG.md`
   - Creates or updates a Release PR
3. **Publishing**: When you merge the Release PR, release-plz automatically:
   - Publishes crates to crates.io in the correct dependency order
   - Creates git tags for each package
   - Creates a GitHub release with changelog

## 📝 Commit Format

Use [Conventional Commits](https://www.conventionalcommits.org/) format:

```
<type>[optional scope]: <description>

[optional body]

[optional footer(s)]
```

### Examples:

- `feat: add texture compression support` → Minor version bump
- `fix: resolve memory leak in asset loading` → Patch version bump  
- `feat!: change API for asset parsing` → Major version bump
- `docs: update README with new examples` → No version bump

### Types:
- `feat`: New features (minor version bump)
- `fix`: Bug fixes (patch version bump)
- `docs`: Documentation changes (no version bump)
- `style`: Code style changes (no version bump)
- `refactor`: Code refactoring (no version bump)
- `test`: Test changes (no version bump)
- `chore`: Maintenance tasks (no version bump)

Add `!` after the type for breaking changes (major version bump).

## 🔧 Configuration

The release process is configured in `release-plz.toml`:

- **Workspace settings**: Applied to all packages by default
- **Package-specific settings**: Override workspace settings for individual packages
- **Changelog settings**: Control how changelogs are generated

## 📦 Package Release Order

Packages are automatically released in dependency order:

1. `unity-asset-core` (no dependencies)
2. `unity-asset-yaml` (depends on core)
3. `unity-asset-binary` (depends on core)
4. `unity-asset` (depends on all sub-crates)
5. `unity-asset-cli` (depends on main library)

## 🏷️ Tagging Strategy

- Individual packages: `{package}-v{version}` (e.g., `unity-asset-core-v0.1.0`)
- Main library: `v{version}` (e.g., `v0.1.0`)

## 🎯 Manual Release (if needed)

If you need to manually trigger a release:

1. Install release-plz: `cargo install release-plz`
2. Create release PR: `release-plz release-pr`
3. Review and merge the PR
4. Publish: `release-plz release`

## 🔍 Troubleshooting

- **Release PR not created**: Check that your commits follow conventional format
- **Publishing failed**: Ensure `CARGO_REGISTRY_TOKEN` secret is set correctly
- **Version conflicts**: release-plz handles dependency versions automatically

## 📚 Learn More

- [release-plz documentation](https://release-plz.dev/)
- [Conventional Commits specification](https://www.conventionalcommits.org/)
- [Semantic Versioning](https://semver.org/)
