# WezTerm Windows build script
# Usage: .\build-wezterm.ps1 [-Release] [-Debug] [-Clean] [-Install]

param(
    [switch]$Release = $true,
    [switch]$Debug,
    [switch]$Clean,
    [switch]$Install,
    [switch]$Help
)

$ErrorActionPreference = "Stop"
$ProjectRoot = $PSScriptRoot

function Write-Step { param($msg) Write-Host "[STEP] $msg" -ForegroundColor Cyan }
function Write-Success { param($msg) Write-Host "[OK]   $msg" -ForegroundColor Green }
function Write-WarningMsg { param($msg) Write-Host "[WARN] $msg" -ForegroundColor Yellow }
function Write-ErrorMsg { param($msg) Write-Host "[ERR]  $msg" -ForegroundColor Red }

if ($Help) {
    Write-Host @"
WezTerm Windows build script

Usage:
    .\build-wezterm.ps1                Build Release (default)
    .\build-wezterm.ps1 -Debug         Build Debug
    .\build-wezterm.ps1 -Clean         Clean target first
    .\build-wezterm.ps1 -Install       Copy built binaries to dist-new

Options:
    -Release    Build optimized binaries (default)
    -Debug      Build debug binaries
    -Clean      Run cargo clean before build
    -Install    Copy binaries to dist-new after build
    -Help       Show this help
"@
    exit 0
}

Write-Step "Checking build environment..."

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-ErrorMsg "cargo not found. Please install Rust toolchain."
    exit 1
}
Write-Success "cargo: $(cargo --version)"

$StrawberryPerl = "C:\Strawberry\perl\bin\perl.exe"
if (-not (Test-Path $StrawberryPerl)) {
    Write-WarningMsg "Strawberry Perl not found at $StrawberryPerl"
    Write-WarningMsg "OpenSSL builds may fail depending on local setup."
} else {
    Write-Success "Strawberry Perl: $StrawberryPerl"
}

Set-Location $ProjectRoot

if ($Clean) {
    Write-Step "Cleaning target directory..."
    cargo clean
    Write-Success "Clean completed."
}

$BuildType = if ($Debug) { "" } else { "--release" }
$BuildTypeName = if ($Debug) { "Debug" } else { "Release" }
$TargetDir = if ($Debug) { "target\debug" } else { "target\release" }

Write-Step "Starting $BuildTypeName build..."
$env:PATH = "C:\Strawberry\perl\bin;$env:PATH"

Write-Step "Building wezterm..."
cargo build $BuildType -p wezterm
if ($LASTEXITCODE -ne 0) {
    Write-ErrorMsg "wezterm build failed."
    exit 1
}
Write-Success "wezterm build completed."

Write-Step "Building wezterm-gui..."
cargo build $BuildType -p wezterm-gui
if ($LASTEXITCODE -ne 0) {
    Write-ErrorMsg "wezterm-gui build failed."
    exit 1
}
Write-Success "wezterm-gui build completed."

Write-Step "Building wezterm-mux-server..."
cargo build $BuildType -p wezterm-mux-server
if ($LASTEXITCODE -ne 0) {
    Write-WarningMsg "wezterm-mux-server build failed (non-fatal)."
} else {
    Write-Success "wezterm-mux-server build completed."
}

Write-Host ""
Write-Step "Build artifacts:"
$Executables = @(
    "wezterm.exe",
    "wezterm-gui.exe",
    "wezterm-mux-server.exe"
)
foreach ($exe in $Executables) {
    $path = Join-Path $TargetDir $exe
    if (Test-Path $path) {
        $size = (Get-Item $path).Length / 1MB
        Write-Host "  $exe - $([math]::Round($size, 1)) MB" -ForegroundColor Gray
    }
}

if ($Install) {
    Write-Host ""
    Write-Step "Copying binaries to dist-new..."
    $DistNew = Join-Path $ProjectRoot "dist-new"
    if (-not (Test-Path $DistNew)) {
        New-Item -ItemType Directory -Path $DistNew | Out-Null
    }

    foreach ($exe in $Executables) {
        $src = Join-Path $TargetDir $exe
        if (Test-Path $src) {
            Copy-Item $src $DistNew -Force
            Write-Success "Copied $exe"
        }
    }

    Write-Host ""
    Write-Success "Copied binaries to: $DistNew"
}

Write-Host ""
Write-Success "Build completed."
