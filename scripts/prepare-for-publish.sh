#!/bin/bash
# 准备发布：将 path 依赖改为 version 依赖

set -e

VERSION=$1
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version>"
    echo "Example: $0 0.1.0"
    exit 1
fi

echo "🔧 Preparing for publish with version $VERSION"

# 备份原始文件
echo "📋 Creating backups..."
find . -name "Cargo.toml" -exec cp {} {}.backup \;

# 更新 unity-asset-yaml 的依赖
echo "📦 Updating unity-asset-yaml dependencies..."
sed -i 's|unity-asset-core = { path = "../unity-asset-core", version = ".*" }|unity-asset-core = "'$VERSION'"|g' unity-asset-yaml/Cargo.toml

# 更新 unity-asset-binary 的依赖
echo "📦 Updating unity-asset-binary dependencies..."
sed -i 's|unity-asset-core = { path = "../unity-asset-core", version = ".*" }|unity-asset-core = "'$VERSION'"|g' unity-asset-binary/Cargo.toml

# 更新 unity-asset-lib 的依赖
echo "📦 Updating unity-asset-lib dependencies..."
sed -i 's|unity-asset-core = { path = "../unity-asset-core", version = ".*" }|unity-asset-core = "'$VERSION'"|g' unity-asset-lib/Cargo.toml
sed -i 's|unity-asset-yaml = { path = "../unity-asset-yaml", version = ".*" }|unity-asset-yaml = "'$VERSION'"|g' unity-asset-lib/Cargo.toml
sed -i 's|unity-asset-binary = { path = "../unity-asset-binary", version = ".*" }|unity-asset-binary = "'$VERSION'"|g' unity-asset-lib/Cargo.toml

# 更新 unity-asset-cli 的依赖
echo "📦 Updating unity-asset-cli dependencies..."
sed -i 's|unity-asset = { path = "../unity-asset-lib", version = ".*" }|unity-asset = "'$VERSION'"|g' unity-asset-cli/Cargo.toml

echo "✅ Dependencies updated for publishing"
echo "🔍 Verifying changes..."

# 验证更改
echo "📋 Changed files:"
find . -name "Cargo.toml" -not -path "./target/*" -exec echo "  {}" \; -exec grep -H "version.*$VERSION" {} \; || true

echo ""
echo "🚀 Ready for publishing!"
echo "💡 To restore original dependencies after publishing, run:"
echo "   find . -name 'Cargo.toml.backup' -exec bash -c 'mv \"\$1\" \"\${1%.backup}\"' _ {} \;"
