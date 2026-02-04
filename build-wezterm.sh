#!/bin/bash
# WezTerm Windows 编译脚本 (Git Bash 版本)
# 用法: ./build-wezterm.sh [--release|--debug] [--clean] [--install]

set -e

PROJECT_ROOT="$(cd "$(dirname "$0")" && pwd)"
cd "$PROJECT_ROOT"

# 颜色
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

step() { echo -e "${CYAN}▶ $1${NC}"; }
success() { echo -e "${GREEN}✓ $1${NC}"; }
warn() { echo -e "${YELLOW}⚠ $1${NC}"; }
error() { echo -e "${RED}✗ $1${NC}"; }

# 默认参数
BUILD_TYPE="--release"
BUILD_NAME="Release"
TARGET_DIR="target/release"
CLEAN=false
INSTALL=false

# 解析参数
while [[ $# -gt 0 ]]; do
    case $1 in
        --debug)
            BUILD_TYPE=""
            BUILD_NAME="Debug"
            TARGET_DIR="target/debug"
            shift
            ;;
        --release)
            BUILD_TYPE="--release"
            BUILD_NAME="Release"
            TARGET_DIR="target/release"
            shift
            ;;
        --clean)
            CLEAN=true
            shift
            ;;
        --install)
            INSTALL=true
            shift
            ;;
        --help|-h)
            cat << EOF
WezTerm Windows 编译脚本

用法:
    ./build-wezterm.sh              # 编译 Release 版本
    ./build-wezterm.sh --debug      # 编译 Debug 版本
    ./build-wezterm.sh --clean      # 清理后重新编译
    ./build-wezterm.sh --install    # 编译并安装到 dist-new

选项:
    --release   编译优化版本 (默认)
    --debug     编译调试版本
    --clean     清理 target 目录后编译
    --install   编译后复制到 dist-new 目录
    --help      显示此帮助信息

注意:
    - 需要 Rust 工具链 (cargo)
    - 需要 Strawberry Perl (用于 OpenSSL 编译)
    - 编译大约需要 1-2 分钟
EOF
            exit 0
            ;;
        *)
            error "未知参数: $1"
            exit 1
            ;;
    esac
done

# 检查依赖
step "检查编译环境..."

if ! command -v cargo &> /dev/null; then
    error "未找到 cargo，请安装 Rust 工具链"
    exit 1
fi
success "cargo: $(cargo --version)"

# 检查 Strawberry Perl
STRAWBERRY_PERL="/c/Strawberry/perl/bin/perl.exe"
if [[ -f "$STRAWBERRY_PERL" ]]; then
    success "Strawberry Perl: $STRAWBERRY_PERL"
    # 将 Strawberry Perl 添加到 PATH 前面
    export PATH="/c/Strawberry/perl/bin:$PATH"
else
    warn "未找到 Strawberry Perl ($STRAWBERRY_PERL)"
    warn "OpenSSL 编译可能会失败"
fi

# 清理
if [[ "$CLEAN" == true ]]; then
    step "清理 target 目录..."
    cargo clean
    success "清理完成"
fi

step "开始编译 $BUILD_NAME 版本..."

# 编译 wezterm
step "编译 wezterm..."
cargo build $BUILD_TYPE -p wezterm
success "wezterm 编译完成"

# 编译 wezterm-gui
step "编译 wezterm-gui..."
cargo build $BUILD_TYPE -p wezterm-gui
success "wezterm-gui 编译完成"

# 编译 wezterm-mux-server
step "编译 wezterm-mux-server..."
if cargo build $BUILD_TYPE -p wezterm-mux-server; then
    success "wezterm-mux-server 编译完成"
else
    warn "wezterm-mux-server 编译失败 (非关键)"
fi

# 显示编译结果
echo ""
step "编译产物:"
for exe in wezterm.exe wezterm-gui.exe wezterm-mux-server.exe; do
    path="$TARGET_DIR/$exe"
    if [[ -f "$path" ]]; then
        size=$(ls -lh "$path" | awk '{print $5}')
        echo "  $exe - $size"
    fi
done

# 安装到 dist-new
if [[ "$INSTALL" == true ]]; then
    echo ""
    step "复制到 dist-new 目录..."

    DIST_NEW="$PROJECT_ROOT/dist-new"
    mkdir -p "$DIST_NEW"

    for exe in wezterm.exe wezterm-gui.exe wezterm-mux-server.exe; do
        src="$TARGET_DIR/$exe"
        if [[ -f "$src" ]]; then
            cp "$src" "$DIST_NEW/"
            success "已复制 $exe"
        fi
    done

    echo ""
    success "文件已复制到: $DIST_NEW"
    echo ""
    echo -e "${YELLOW}替换步骤:${NC}"
    echo "  1. 关闭当前 WezTerm"
    echo "  2. 执行: cp $DIST_NEW/*.exe $PROJECT_ROOT/dist/"
    echo "  3. 启动新版 WezTerm"
fi

echo ""
success "编译完成！"
