# Builds the release executable, runs the tests and puts the release in
# its own versioned folder containing only the program:
#   dist\WinSearch-<version>\winsearch.exe
#   powershell -ExecutionPolicy Bypass -File scripts\package.ps1 [-SkipTests]
param([switch]$SkipTests)
$ErrorActionPreference = "Stop"
$root = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $root

$version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
Write-Host "Building WinSearch $version (release)..."
cargo build --release --bin winsearch
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
if (-not $SkipTests) {
    Write-Host "Running tests..."
    cargo test --release
    if ($LASTEXITCODE -ne 0) { throw "tests failed" }
}

$exe = Join-Path $root "target\release\winsearch.exe"
$info = (Get-Item $exe).VersionInfo
if ($info.ProductVersion -ne $version) { throw "embedded version '$($info.ProductVersion)' does not match $version" }

$dir = Join-Path $root "dist\WinSearch-$version"
if (Test-Path $dir) { throw "$dir already exists; bump the version or remove it first" }
New-Item -ItemType Directory -Force $dir | Out-Null
Copy-Item $exe $dir
$out = Get-Item (Join-Path $dir "winsearch.exe")
$hash = (Get-FileHash $out.FullName -Algorithm SHA256).Hash
Write-Host ("{0}  {1:N0} bytes  SHA256 {2}" -f $out.FullName, $out.Length, $hash)
