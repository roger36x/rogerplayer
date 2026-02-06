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

# 准备图标
echo "[0/4] Preparing App Icon..."
ICON_SOURCE="$PROJECT_DIR/r-alphabet-round-circle-icon.webp"
COMMON_ICON_PATH="$TARGET_DIR/AppIcon.icns"

if [ -f "$ICON_SOURCE" ]; then
    echo "Generating AppIcon from $ICON_SOURCE..."
    ICONSET_DIR="$TARGET_DIR/AppIcon.iconset"
    mkdir -p "$ICONSET_DIR"

    # Handle WebP source by creating a temporary PNG master
    PROCESS_SOURCE="$ICON_SOURCE"
    if [[ "$ICON_SOURCE" == *.webp ]]; then
        echo "Converting WebP to PNG master..."
        sips -s format png "$ICON_SOURCE" --out "$ICONSET_DIR/master.png" > /dev/null
        PROCESS_SOURCE="$ICONSET_DIR/master.png"
    fi

    # Standard sizes
    sips -z 16 16     "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_16x16.png" > /dev/null
    sips -z 32 32     "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_16x16@2x.png" > /dev/null
    sips -z 32 32     "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_32x32.png" > /dev/null
    sips -z 64 64     "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_32x32@2x.png" > /dev/null
    sips -z 128 128   "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_128x128.png" > /dev/null
    sips -z 256 256   "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_128x128@2x.png" > /dev/null
    sips -z 256 256   "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_256x256.png" > /dev/null
    sips -z 512 512   "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_256x256@2x.png" > /dev/null
    sips -z 512 512   "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_512x512.png" > /dev/null
    sips -z 1024 1024 "$PROCESS_SOURCE" --out "$ICONSET_DIR/icon_512x512@2x.png" > /dev/null

    iconutil -c icns "$ICONSET_DIR" -o "$COMMON_ICON_PATH"
    rm -rf "$ICONSET_DIR"
elif [ -f "$PROJECT_DIR/assets/AppIcon.icns" ]; then
    echo "Using existing AppIcon.icns from assets..."
    cp "$PROJECT_DIR/assets/AppIcon.icns" "$COMMON_ICON_PATH"
fi

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

    # 复制图标
    if [ -f "$COMMON_ICON_PATH" ]; then
        cp "$COMMON_ICON_PATH" "$APP_DIR/Contents/Resources/AppIcon.icns"
    fi

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
    do script "cd ~ && '$BINARY' tui $ARGS; exit"
    set playerWindow to front window
    repeat
        delay 0.5
        try
            if not busy of playerWindow then
                close playerWindow
                exit repeat
            end if
        on error
            exit repeat
        end try
    end repeat
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
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
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
    # 7. Ad-hoc code signing
    echo "Signing $ARCH_NAME app bundle..."
    codesign --force --deep --sign - "$APP_DIR"
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
