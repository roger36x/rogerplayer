#!/bin/bash
# Build Roger Player for both Apple Silicon and Intel
# Usage: ./scripts/build-all.sh

set -e

APP_NAME="Roger Player"
BUNDLE_ID="com.roger.player"
VERSION="0.1.0"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
TARGET_DIR="$PROJECT_DIR/target"

echo "========================================"
echo "  Roger Player - Universal Build"
echo "========================================"
echo ""

# 编译两个架构
echo "[1/4] Building for Apple Silicon (arm64)..."
cd "$PROJECT_DIR"
cargo build --release --target aarch64-apple-darwin

echo "[2/4] Building for Intel (x86_64)..."
cargo build --release --target x86_64-apple-darwin

# 创建 app bundle 的函数
create_app_bundle() {
    local ARCH_NAME="$1"
    local TARGET="$2"
    local MIN_OS="$3"
    local APP_DIR="$TARGET_DIR/$APP_NAME ($ARCH_NAME).app"

    echo "Creating $ARCH_NAME app bundle..."

    rm -rf "$APP_DIR"
    mkdir -p "$APP_DIR/Contents/MacOS"
    mkdir -p "$APP_DIR/Contents/Resources"

    # 复制二进制
    cp "$TARGET_DIR/$TARGET/release/roger-player" "$APP_DIR/Contents/MacOS/"

    # 创建 launcher
    cat > "$APP_DIR/Contents/MacOS/launcher" << 'LAUNCHER_EOF'
#!/bin/bash
DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$DIR/roger-player"
ARGS=""
if [ $# -gt 0 ]; then
    ARGS="$@"
fi
osascript <<APPLESCRIPT
tell application "Terminal"
    activate
    set newTab to do script "cd ~ && '$BINARY' tui $ARGS; exit"
end tell
APPLESCRIPT
LAUNCHER_EOF
    chmod +x "$APP_DIR/Contents/MacOS/launcher"

    # 创建 Info.plist
    cat > "$APP_DIR/Contents/Info.plist" << PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>launcher</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_OS</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Audio File</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.audio</string>
                <string>org.xiph.flac</string>
                <string>public.mp3</string>
                <string>com.microsoft.waveform-audio</string>
                <string>public.aiff-audio</string>
            </array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key>
            <string>Folder</string>
            <key>CFBundleTypeRole</key>
            <string>Viewer</string>
            <key>LSHandlerRank</key>
            <string>Alternate</string>
            <key>LSItemContentTypes</key>
            <array>
                <string>public.folder</string>
            </array>
        </dict>
    </array>
</dict>
</plist>
PLIST_EOF
}

# 创建两个 app bundle
echo ""
echo "[3/4] Creating Apple Silicon app bundle..."
create_app_bundle "Apple Silicon" "aarch64-apple-darwin" "11.0"

echo "[4/4] Creating Intel app bundle..."
create_app_bundle "Intel" "x86_64-apple-darwin" "10.15"

# 显示结果
echo ""
echo "========================================"
echo "  Build Complete!"
echo "========================================"
echo ""
echo "Generated apps:"
echo "  - $TARGET_DIR/$APP_NAME (Apple Silicon).app"
echo "  - $TARGET_DIR/$APP_NAME (Intel).app"
echo ""
echo "Binary info:"
file "$TARGET_DIR/$APP_NAME (Apple Silicon).app/Contents/MacOS/roger-player"
file "$TARGET_DIR/$APP_NAME (Intel).app/Contents/MacOS/roger-player"
echo ""
echo "To install:"
echo "  cp -r \"$TARGET_DIR/$APP_NAME (Apple Silicon).app\" /Applications/"
echo "  cp -r \"$TARGET_DIR/$APP_NAME (Intel).app\" /Applications/"
