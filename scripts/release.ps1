# PowerShell script for Windows release management
param(
    [Parameter(Mandatory=$true)]
    [string]$Version,
    
    [switch]$DryRun
)

Write-Host "🚀 Unity Asset Parser Release Script" -ForegroundColor Green
Write-Host "Version: $Version" -ForegroundColor Yellow

if ($DryRun) {
    Write-Host "🔍 DRY RUN MODE - No actual changes will be made" -ForegroundColor Cyan
}

# 验证版本格式
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    Write-Error "Invalid version format. Use semantic versioning (e.g., 1.0.0)"
    exit 1
}

# 检查工作目录是否干净
$gitStatus = git status --porcelain
if ($gitStatus -and -not $DryRun) {
    Write-Error "Working directory is not clean. Please commit or stash changes first."
    exit 1
}

Write-Host "📋 Pre-release checks..." -ForegroundColor Blue

# 运行测试
Write-Host "🧪 Running tests..."
if (-not $DryRun) {
    cargo test --all --all-features
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Tests failed!"
        exit 1
    }
}

# 运行 clippy
Write-Host "📎 Running clippy..."
if (-not $DryRun) {
    cargo clippy --all-targets --all-features -- -D warnings
    if ($LASTEXITCODE -ne 0) {
        Write-Error "Clippy checks failed!"
        exit 1
    }
}

# 更新版本号
Write-Host "📝 Updating version numbers..." -ForegroundColor Blue
$cargoFiles = Get-ChildItem -Recurse -Name "Cargo.toml" | Where-Object { $_ -notlike "target*" }

foreach ($file in $cargoFiles) {
    Write-Host "  Updating $file"
    if (-not $DryRun) {
        (Get-Content $file) -replace 'version = "0\.1\.0"', "version = `"$Version`"" | Set-Content $file
    }
}

# 更新 CHANGELOG.md
Write-Host "📝 Updating CHANGELOG.md..." -ForegroundColor Blue
if (-not $DryRun) {
    $changelogContent = Get-Content "CHANGELOG.md"
    $today = Get-Date -Format 'yyyy-MM-dd'

    # 替换 [Unreleased] 为当前版本
    $newChangelog = $changelogContent -replace '\[Unreleased\]', "[$Version] - $today"

    # 替换 TBD 日期为实际日期
    $newChangelog = $newChangelog -replace '\[0\.1\.0\] - TBD \(First Release\)', "[$Version] - $today"

    # 在顶部添加新的 Unreleased 部分
    $unreleasedSection = @(
        "## [Unreleased]",
        "",
        "### Added",
        "- Nothing yet",
        "",
        "### Changed",
        "- Nothing yet",
        "",
        "### Fixed",
        "- Nothing yet",
        ""
    )

    # 找到第一个版本标题的位置并插入新的 Unreleased 部分
    $insertIndex = -1
    for ($i = 0; $i -lt $newChangelog.Length; $i++) {
        if ($newChangelog[$i] -match "^## \[$Version\]") {
            $insertIndex = $i
            break
        }
    }

    if ($insertIndex -gt 0) {
        $finalChangelog = $newChangelog[0..($insertIndex-1)] + $unreleasedSection + $newChangelog[$insertIndex..($newChangelog.Length-1)]
        $finalChangelog | Set-Content "CHANGELOG.md"
    } else {
        $newChangelog | Set-Content "CHANGELOG.md"
    }
}

# 提交更改
if (-not $DryRun) {
    Write-Host "📝 Committing version bump..." -ForegroundColor Blue
    git add .
    git commit -m "chore: bump version to $Version"
    
    # 创建标签
    Write-Host "🏷️  Creating tag v$Version..." -ForegroundColor Blue
    git tag -a "v$Version" -m "Release version $Version"
    
    Write-Host "✅ Release prepared!" -ForegroundColor Green
    Write-Host ""
    Write-Host "Next steps:" -ForegroundColor Yellow
    Write-Host "1. Review the changes: git show HEAD" -ForegroundColor White
    Write-Host "2. Push to trigger release: git push origin main --tags" -ForegroundColor White
    Write-Host ""
    Write-Host "The GitHub Actions workflow will automatically:" -ForegroundColor Cyan
    Write-Host "  • Run full test suite" -ForegroundColor White
    Write-Host "  • Publish all crates to crates.io in correct order" -ForegroundColor White
    Write-Host "  • Create GitHub release with changelog" -ForegroundColor White
} else {
    Write-Host "✅ Dry run completed successfully!" -ForegroundColor Green
    Write-Host "Run without -DryRun to actually perform the release." -ForegroundColor Yellow
}
