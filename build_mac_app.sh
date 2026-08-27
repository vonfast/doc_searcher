#!/bin/bash
set -e

echo "=== Building macOS App Bundle (DoXsearch.app) ==="

APP_NAME="DoXsearch"
BUNDLE_DIR="target/$APP_NAME.app"
CONTENTS_DIR="$BUNDLE_DIR/Contents"
MACOS_DIR="$CONTENTS_DIR/MacOS"
RESOURCES_DIR="$CONTENTS_DIR/Resources"

TARGET=${1:-""}

mkdir -p "$MACOS_DIR"
mkdir -p "$RESOURCES_DIR"

if [ -n "$TARGET" ] && [ -f "target/$TARGET/release/doxsearch" ]; then
    BINARY_PATH="target/$TARGET/release/doxsearch"
elif [ -f "target/release/doxsearch" ]; then
    BINARY_PATH="target/release/doxsearch"
else
    echo "Warning: Binary not found yet. Searching in target/..."
    BINARY_PATH=$(find target/ -name "doxsearch" -type f | head -n 1)
fi

if [ -f "$BINARY_PATH" ]; then
    cp "$BINARY_PATH" "$MACOS_DIR/doxsearch"
    chmod +x "$MACOS_DIR/doxsearch"
    echo "Copied binary from $BINARY_PATH"
else
    echo "Executable binary missing. Run cargo build --release first." >&2
    exit 1
fi

cat <<EOF > "$CONTENTS_DIR/Info.plist"
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleExecutable</key>
    <string>doxsearch</string>
    <key>CFBundleIconFile</key>
    <string>doxsearch.png</string>
    <key>CFBundleIdentifier</key>
    <string>com.vonfast.doxsearch</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>DoXsearch</string>
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

if [ -f "assets/doxsearch.png" ]; then
    cp assets/doxsearch.png "$RESOURCES_DIR/doxsearch.png"
elif [ -f "assets/doxgrep.png" ]; then
    cp assets/doxgrep.png "$RESOURCES_DIR/doxsearch.png"
fi

if command -v codesign &> /dev/null && [ -d "$BUNDLE_DIR" ]; then
    echo "--- Applying ad-hoc code signature ---"
    codesign -s - --force --deep "$BUNDLE_DIR"
fi

if command -v zip &> /dev/null && [ -d "$BUNDLE_DIR" ]; then
    cd target
    zip -r -q "../DoXsearch-macOS.zip" "$APP_NAME.app"
    cd ..
    echo "--- Created DoXsearch-macOS.zip containing $APP_NAME.app ---"
fi

if command -v hdiutil &> /dev/null && [ -d "$BUNDLE_DIR" ]; then
    echo "--- Creating DMG disk image ---"
    DMG_DIR="target/dmg_stage"
    rm -rf "$DMG_DIR"
    mkdir -p "$DMG_DIR"
    cp -R "$BUNDLE_DIR" "$DMG_DIR/"
    ln -s /Applications "$DMG_DIR/Applications"
    hdiutil create -volname "DoXsearch" -srcfolder "$DMG_DIR" -ov -format UDZO "DoXsearch-macOS.dmg"
    rm -rf "$DMG_DIR"
    echo "--- Created DoXsearch-macOS.dmg ---"
fi
