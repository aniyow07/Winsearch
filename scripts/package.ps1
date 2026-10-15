# Builds the release binaries, runs the tests and creates a portable zip:
#   powershell -ExecutionPolicy Bypass -File scripts\package.ps1 [-SkipTests]
param([switch]$SkipTests)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Building WinSearch $version (release)..."
cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
if (-not $SkipTests) {
    Write-Host "Running tests..."
    cargo test --release
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }
}

$name = "WinSearch-$version-win-x64"
$dist = Join-Path $root "dist"
$stage = Join-Path $dist $name
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force (Join-Path $stage "docs") | Out-Null
Copy-Item target\release\winsearch.exe, target\release\wsctl.exe $stage
Copy-Item README.md $stage
Copy-Item docs\*.md (Join-Path $stage "docs")
$zip = Join-Path $dist "$name.zip"
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $stage -DestinationPath $zip
Get-Item $zip, (Join-Path $stage "winsearch.exe") | Select-Object FullName, Length
