# WezTerm Windows 编译脚本
# 用法: .\build-wezterm.ps1 [-Release] [-Debug] [-Clean] [-Install]

param(
    [switch]$Release = $true,
    [switch]$Debug,
    [switch]$Clean,
    [switch]$Install,
    [switch]$Help
)

$ErrorActionPreference = "Stop"
$ProjectRoot = $PSScriptRoot

# 颜色输出
function Write-Step { param($msg) Write-Host "▶ $msg" -ForegroundColor Cyan }
function Write-Success { param($msg) Write-Host "✓ $msg" -ForegroundColor Green }
function Write-Warning { param($msg) Write-Host "⚠ $msg" -ForegroundColor Yellow }
function Write-Error { param($msg) Write-Host "✗ $msg" -ForegroundColor Red }

if ($Help) {
    Write-Host @"
WezTerm Windows 编译脚本

用法:
    .\build-wezterm.ps1              # 编译 Release 版本
    .\build-wezterm.ps1 -Debug       # 编译 Debug 版本
    .\build-wezterm.ps1 -Clean       # 清理后重新编译
    .\build-wezterm.ps1 -Install     # 编译并安装到 dist 目录

选项:
    -Release    编译优化版本 (默认)
    -Debug      编译调试版本
    -Clean      清理 target 目录后编译
    -Install    编译后复制到 dist-new 目录
    -Help       显示此帮助信息

注意:
    - 需要 Rust 工具链 (cargo)
    - 需要 Strawberry Perl (用于 OpenSSL 编译)
    - 编译大约需要 1-2 分钟
"@
    exit 0
}

# 检查依赖
Write-Step "检查编译环境..."

# 检查 cargo
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Write-Error "未找到 cargo，请安装 Rust 工具链"
    exit 1
}
Write-Success "cargo: $(cargo --version)"

# 检查 Strawberry Perl
$StrawberryPerl = "C:\Strawberry\perl\bin\perl.exe"
if (-not (Test-Path $StrawberryPerl)) {
    Write-Warning "未找到 Strawberry Perl ($StrawberryPerl)"
    Write-Warning "OpenSSL 编译可能会失败"
} else {
    Write-Success "Strawberry Perl: $StrawberryPerl"
}

# 切换到项目目录
Set-Location $ProjectRoot

# 清理
if ($Clean) {
    Write-Step "清理 target 目录..."
    cargo clean
    Write-Success "清理完成"
}

# 确定编译配置
$BuildType = if ($Debug) { "" } else { "--release" }
$BuildTypeName = if ($Debug) { "Debug" } else { "Release" }
$TargetDir = if ($Debug) { "target\debug" } else { "target\release" }

Write-Step "开始编译 $BuildTypeName 版本..."

# 编译 wezterm (CLI)
Write-Step "编译 wezterm..."
$env:PATH = "C:\Strawberry\perl\bin;$env:PATH"
cargo build $BuildType -p wezterm
if ($LASTEXITCODE -ne 0) {
    Write-Error "wezterm 编译失败"
    exit 1
}
Write-Success "wezterm 编译完成"

# 编译 wezterm-gui
Write-Step "编译 wezterm-gui..."
cargo build $BuildType -p wezterm-gui
if ($LASTEXITCODE -ne 0) {
    Write-Error "wezterm-gui 编译失败"
    exit 1
}
Write-Success "wezterm-gui 编译完成"

# 编译 wezterm-mux-server (可选)
Write-Step "编译 wezterm-mux-server..."
cargo build $BuildType -p wezterm-mux-server
if ($LASTEXITCODE -ne 0) {
    Write-Warning "wezterm-mux-server 编译失败 (非关键)"
} else {
    Write-Success "wezterm-mux-server 编译完成"
}

# 显示编译结果
Write-Host ""
Write-Step "编译产物:"
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

# 安装到 dist-new
if ($Install) {
    Write-Host ""
    Write-Step "复制到 dist-new 目录..."

    $DistNew = Join-Path $ProjectRoot "dist-new"
    if (-not (Test-Path $DistNew)) {
        New-Item -ItemType Directory -Path $DistNew | Out-Null
    }

    foreach ($exe in $Executables) {
        $src = Join-Path $TargetDir $exe
        if (Test-Path $src) {
            Copy-Item $src $DistNew -Force
            Write-Success "已复制 $exe"
        }
    }

    Write-Host ""
    Write-Success "文件已复制到: $DistNew"
    Write-Host ""
    Write-Host "替换步骤:" -ForegroundColor Yellow
    Write-Host "  1. 关闭当前 WezTerm"
    Write-Host "  2. 执行: copy $DistNew\*.exe $ProjectRoot\dist\"
    Write-Host "  3. 启动新版 WezTerm"
}

Write-Host ""
Write-Success "编译完成！"
