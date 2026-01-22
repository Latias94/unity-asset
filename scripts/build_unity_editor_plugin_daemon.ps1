#!/usr/bin/env pwsh
param(
  [ValidateSet("Debug", "Release")]
  [string]$Profile = "Release"
)

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Set-Location $repoRoot

$targetDir = Join-Path $repoRoot "repo-ref/UnityAssetHero/Tools~/win-x64"
New-Item -ItemType Directory -Force -Path $targetDir | Out-Null

$cargoProfileArgs = @()
if ($Profile -eq "Release") {
  $cargoProfileArgs += "--release"
}

Write-Host "Building unity-asset-search-daemon ($Profile)..."
cargo build -p unity-asset-search-daemon @cargoProfileArgs

$exeName = "unity-asset-search-daemon.exe"
$builtExe = Join-Path $repoRoot "target/$($Profile.ToLower())/$exeName"
if (!(Test-Path $builtExe)) {
  throw "Built binary not found: $builtExe"
}

$destExe = Join-Path $targetDir $exeName
Copy-Item -Force $builtExe $destExe
Write-Host "Copied: $builtExe -> $destExe"
