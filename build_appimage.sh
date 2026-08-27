#!/bin/bash
set -e

# Check for appimagetool
if ! command -v appimagetool &> /dev/null && [ ! -f "./appimagetool-x86_64.AppImage" ]; then
    echo "Error: appimagetool missing."
    echo "Download it: wget https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage"
    echo "Make it executable: chmod +x appimagetool-x86_64.AppImage"
    exit 1
fi

APP_TOOL="./appimagetool-x86_64.AppImage"
if command -v appimagetool &> /dev/null; then
    APP_TOOL="appimagetool"
fi

# 1. Build release
echo "--- Building release build ---"
cargo build --release

# 2. Prepare AppDir
APPDIR="target/doxsearch.AppDir"
echo "--- Preparing AppDir: $APPDIR ---"
mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"

# Copy binary
cp target/release/doxsearch "$APPDIR/usr/bin/"

# Copy desktop file
cp doxsearch.desktop "$APPDIR/usr/share/applications/"
cp doxsearch.desktop "$APPDIR/"

# Copy icon (if exists)
if [ -f "assets/doxsearch.png" ]; then
    cp assets/doxsearch.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/doxsearch.png"
    cp assets/doxsearch.png "$APPDIR/doxsearch.png"
elif [ -f "assets/doxgrep.png" ]; then
    cp assets/doxgrep.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/doxsearch.png"
    cp assets/doxgrep.png "$APPDIR/doxsearch.png"
else
    echo "Warning: assets/doxsearch.png missing. Creating a dummy file so appimagetool can proceed."
    touch "$APPDIR/doxsearch.png"
fi

# Create AppRun (symlink to the binary)
ln -sf usr/bin/doxsearch "$APPDIR/AppRun"

# 3. Build AppImage
echo "--- Building AppImage ---"
export APPIMAGE_EXTRACT_AND_RUN=1
ARCH=x86_64 $APP_TOOL "$APPDIR" DoXsearch-x86_64.AppImage

echo "--- Done: DoXsearch-x86_64.AppImage ---"
