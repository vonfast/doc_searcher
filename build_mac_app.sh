#!/bin/bash
set -e

echo "=== Building macOS App Bundle (DoXgrep.app) ==="

APP_NAME="DoXgrep"
BUNDLE_DIR="target/$APP_NAME.app"
CONTENTS_DIR="$BUNDLE_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

TARGET=${1:-""}

mkdir -p "$MACOS_DIR"
mkdir -p "$RESOURCES_DIR"

if [ -n "$TARGET" ] && [ -f "target/$TARGET/release/doxgrep" ]; then
    BINARY_PATH="target/$TARGET/release/doxgrep"
elif [ -f "target/release/doxgrep" ]; then
    BINARY_PATH="target/release/doxgrep"
else
    echo "Warning: Binary not found yet. Searching in target/..."
    BINARY_PATH=$(find target/ -name "doxgrep" -type f | head -n 1)
fi

if [ -f "$BINARY_PATH" ]; then
    cp "$BINARY_PATH" "$MACOS_DIR/doxgrep"
    chmod +x "$MACOS_DIR/doxgrep"
    echo "Copied binary from $BINARY_PATH"
else
    echo "Executable binary missing. Run cargo build --release first."
fi

cat <<EOF > "$CONTENTS_DIR/Info.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>doxgrep</string>
    <key>CFBundleIconFile</key>
    <string>doxgrep.icns</string>
    <key>CFBundleIdentifier</key>
    <string>com.vonfast.doxgrep</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>DoXgrep</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.2.0</string>
    <key>CFBundleVersion</key>
    <string>0.2.0</string>
    <key>LSMinimumSystemVersion</key>
    <string>10.15</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

if [ -f "assets/doxgrep.png" ]; then
    cp assets/doxgrep.png "$RESOURCES_DIR/doxgrep.png"
fi

if command -v zip &> /dev/null && [ -d "$BUNDLE_DIR" ]; then
    cd target
    zip -r -q "../DoXgrep-macOS.zip" "$APP_NAME.app"
    cd ..
    echo "--- Created DoXgrep-macOS.zip containing $APP_NAME.app ---"
fi
