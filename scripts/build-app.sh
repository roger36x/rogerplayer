#!/bin/bash
# Build Roger Player as macOS app bundle
# Usage: ./scripts/build-app.sh

set -e

APP_NAME="Roger Player"
BUNDLE_ID="com.roger.player"
VERSION="0.1.0"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
BUILD_DIR="$PROJECT_DIR/target/release"
APP_DIR="$PROJECT_DIR/target/$APP_NAME.app"

echo "Building Roger Player..."

# 1. Build release binary
cd "$PROJECT_DIR"
cargo build --release

# 2. Create app bundle structure
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

# 3. Copy binary
cp "$BUILD_DIR/roger-player" "$APP_DIR/Contents/MacOS/"

# 4. Create launcher script (opens Terminal with TUI)
cat > "$APP_DIR/Contents/MacOS/launcher" << 'EOF'
#!/bin/bash
# Get the directory where this script is located
DIR="$(cd "$(dirname "$0")" && pwd)"
BINARY="$DIR/roger-player"

# Get dropped files (if any)
ARGS=""
if [ $# -gt 0 ]; then
    ARGS="$@"
fi

# Open Terminal and run the TUI, close window on exit
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
EOF
chmod +x "$APP_DIR/Contents/MacOS/launcher"

# 5. Setup Icon
ICON_SOURCE="$PROJECT_DIR/r-alphabet-round-circle-icon.webp"
if [ -f "$ICON_SOURCE" ]; then
    echo "Generating AppIcon from $ICON_SOURCE..."
    ICONSET_DIR="$PROJECT_DIR/target/AppIcon.iconset"
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

    iconutil -c icns "$ICONSET_DIR" -o "$APP_DIR/Contents/Resources/AppIcon.icns"
    rm -rf "$ICONSET_DIR"
elif [ -f "$PROJECT_DIR/assets/AppIcon.icns" ]; then
    echo "Using existing AppIcon.icns from assets..."
    cp "$PROJECT_DIR/assets/AppIcon.icns" "$APP_DIR/Contents/Resources/"
fi

# 6. Create Info.plist
cat > "$APP_DIR/Contents/Info.plist" << EOF
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
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
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
EOF

# 7. Ad-hoc code signing (Required for Apple Silicon and to remove "corrupted" warning)
echo "Signing app bundle..."
codesign --force --deep --sign - "$APP_DIR"

echo ""
echo "✅ Build complete!"
echo ""
echo "App location: $APP_DIR"
echo ""
echo "To install:"
echo "  cp -r \"$APP_DIR\" /Applications/"
echo ""
echo "Or open directly:"
echo "  open \"$APP_DIR\""
