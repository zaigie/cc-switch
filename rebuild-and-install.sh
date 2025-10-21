#!/bin/bash

# CC-Switch 自动重新编译和安装脚本
# 用途：编译最新代码并替换现有的应用程序

set -e  # 遇到错误立即退出

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# 应用名称和路径
APP_NAME="CC-Switch"
APP_PATH="/Applications/${APP_NAME}.app"
BUILD_OUTPUT=""  # 将在构建后动态确定

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}  CC-Switch 自动重新编译和安装脚本${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""

# 检查是否在项目根目录
if [ ! -f "package.json" ] || [ ! -d "src-tauri" ]; then
    echo -e "${RED}❌ 错误：请在项目根目录运行此脚本${NC}"
    exit 1
fi

# 检查必需的工具
echo -e "${YELLOW}🔍 检查开发环境...${NC}"

if ! command -v pnpm &> /dev/null; then
    echo -e "${RED}❌ 错误：未找到 pnpm，请先安装 pnpm${NC}"
    echo -e "${YELLOW}安装命令: npm install -g pnpm${NC}"
    exit 1
fi

if ! command -v cargo &> /dev/null; then
    echo -e "${RED}❌ 错误：未找到 Rust/Cargo，请先安装 Rust${NC}"
    echo -e "${YELLOW}安装命令: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh${NC}"
    exit 1
fi

# 检查并添加 macOS targets (与 GitHub Actions 一致)
echo -e "${YELLOW}🔧 检查 Rust targets...${NC}"
REQUIRED_TARGETS=("aarch64-apple-darwin" "x86_64-apple-darwin")
MISSING_TARGETS=()

for target in "${REQUIRED_TARGETS[@]}"; do
    if ! rustup target list | grep -q "^${target} (installed)"; then
        MISSING_TARGETS+=("$target")
    fi
done

if [ ${#MISSING_TARGETS[@]} -gt 0 ]; then
    echo -e "${YELLOW}📥 添加缺失的 Rust targets: ${MISSING_TARGETS[*]}${NC}"
    rustup target add "${MISSING_TARGETS[@]}"
    echo -e "${GREEN}✅ Targets 添加完成${NC}"
else
    echo -e "${GREEN}✅ 所有必需的 targets 已安装${NC}"
fi

echo -e "${GREEN}✅ 环境检查通过${NC}"
echo ""

# 询问是否清理缓存
read -p "是否清理构建缓存？(建议首次构建或遇到问题时选择 y) [y/N]: " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    echo -e "${YELLOW}🧹 清理构建缓存...${NC}"
    cargo clean 2>/dev/null || true
    rm -rf src-tauri/target
    echo -e "${GREEN}✅ 缓存清理完成${NC}"
    echo ""
fi

# 拉取最新代码（可选）
read -p "是否拉取最新代码？[y/N]: " -n 1 -r
echo
if [[ $REPLY =~ ^[Yy]$ ]]; then
    echo -e "${YELLOW}📥 拉取最新代码...${NC}"
    git pull || {
        echo -e "${RED}❌ Git pull 失败，继续使用本地代码${NC}"
    }
    echo ""
fi

# 安装/更新依赖 (与 GitHub Actions 一致使用 --frozen-lockfile)
echo -e "${YELLOW}📦 安装/更新依赖...${NC}"
if [ -f "pnpm-lock.yaml" ]; then
    echo -e "${BLUE}使用 --frozen-lockfile 模式 (与 CI 一致)${NC}"
    pnpm install --frozen-lockfile
else
    echo -e "${YELLOW}⚠️  未找到 pnpm-lock.yaml，使用常规安装${NC}"
    pnpm install
fi

echo ""

# 构建应用 (与 GitHub Actions 一致使用 universal target)
echo -e "${YELLOW}🔨 开始编译应用 (universal-apple-darwin，这可能需要几分钟)...${NC}"
echo -e "${BLUE}提示: 本地构建可能会出现签名错误，但不影响应用使用${NC}"
echo -e "${BLUE}------------------------------------------------${NC}"

# 清除签名相关环境变量（如果存在），本地开发不需要签名
unset TAURI_SIGNING_PRIVATE_KEY
unset TAURI_SIGNING_PRIVATE_KEY_PASSWORD

# 只构建 app bundle，跳过 DMG（本地开发不需要 DMG）
# 使用 || true 忽略签名错误（.app 已生成，签名错误不影响使用）
pnpm tauri build --target universal-apple-darwin --bundles app || true

echo -e "${BLUE}------------------------------------------------${NC}"

echo ""

# 查找构建产物 (与 GitHub Actions 一致的查找逻辑)
echo -e "${YELLOW}🔍 查找构建产物...${NC}"
SEARCH_PATHS=(
    "src-tauri/target/universal-apple-darwin/release/bundle/macos"
    "src-tauri/target/aarch64-apple-darwin/release/bundle/macos"
    "src-tauri/target/x86_64-apple-darwin/release/bundle/macos"
    "src-tauri/target/release/bundle/macos"
)

for path in "${SEARCH_PATHS[@]}"; do
    if [ -d "$path" ]; then
        APP_CANDIDATE=$(find "$path" -maxdepth 1 -name "*.app" -type d | head -1)
        if [ -n "$APP_CANDIDATE" ]; then
            BUILD_OUTPUT="$APP_CANDIDATE"
            echo -e "${GREEN}✅ 找到构建产物: ${BUILD_OUTPUT}${NC}"
            break
        fi
    fi
done

if [ -z "$BUILD_OUTPUT" ] || [ ! -d "$BUILD_OUTPUT" ]; then
    echo -e "${RED}❌ 错误：未找到构建产物${NC}"
    echo -e "${YELLOW}已搜索路径：${NC}"
    for path in "${SEARCH_PATHS[@]}"; do
        echo -e "  - $path"
    done
    exit 1
fi

echo ""

# 备份现有应用（如果存在）
if [ -d "$APP_PATH" ]; then
    BACKUP_PATH="${APP_PATH}.backup.$(date +%Y%m%d_%H%M%S)"
    echo -e "${YELLOW}💾 备份现有应用到: ${BACKUP_PATH}${NC}"
    cp -R "$APP_PATH" "$BACKUP_PATH"
    echo -e "${GREEN}✅ 备份完成${NC}"
    echo ""

    # 关闭正在运行的应用
    echo -e "${YELLOW}🛑 检查并关闭正在运行的应用...${NC}"
    if pgrep -x "$APP_NAME" > /dev/null; then
        osascript -e "quit app \"$APP_NAME\"" 2>/dev/null || killall "$APP_NAME" 2>/dev/null || true
        sleep 2
        echo -e "${GREEN}✅ 应用已关闭${NC}"
    else
        echo -e "${GREEN}✅ 应用未在运行${NC}"
    fi
    echo ""

    # 删除旧应用
    echo -e "${YELLOW}🗑️  删除旧应用...${NC}"
    rm -rf "$APP_PATH"
    echo -e "${GREEN}✅ 旧应用已删除${NC}"
    echo ""
fi

# 安装新应用
echo -e "${YELLOW}📲 安装新应用到 /Applications ...${NC}"
cp -R "$BUILD_OUTPUT" "$APP_PATH"

# 移除隔离属性（避免 macOS 安全提示）
echo -e "${YELLOW}🔓 移除 macOS 隔离属性...${NC}"
xattr -cr "$APP_PATH"

echo -e "${GREEN}✅ 新应用安装完成${NC}"
echo ""

# 显示应用信息
echo -e "${BLUE}========================================${NC}"
echo -e "${GREEN}🎉 安装成功！${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""
echo -e "📍 应用位置: ${GREEN}${APP_PATH}${NC}"
echo -e "📦 应用大小: ${GREEN}$(du -sh "$APP_PATH" | cut -f1)${NC}"
echo ""

# 询问是否立即启动
read -p "是否立即启动应用？[Y/n]: " -n 1 -r
echo
if [[ ! $REPLY =~ ^[Nn]$ ]]; then
    echo -e "${YELLOW}🚀 启动应用...${NC}"
    open "$APP_PATH"
    echo -e "${GREEN}✅ 应用已启动${NC}"
fi

echo ""
echo -e "${BLUE}========================================${NC}"
echo -e "${GREEN}✨ 完成！${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""
echo -e "${YELLOW}提示：${NC}"
echo -e "  • 旧应用已备份，如需回滚可在 /Applications 中找到 .backup 文件"
echo -e "  • 可以在启动台或 /Applications 文件夹中找到应用"
echo -e "  • 如果遇到问题，可以删除应用并恢复备份"
echo ""
