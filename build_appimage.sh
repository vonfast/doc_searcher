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
APPDIR="target/doxgrep.AppDir"
echo "--- Preparing AppDir: $APPDIR ---"
mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"

# Copy binary
cp target/release/doxgrep "$APPDIR/usr/bin/"

# Copy desktop file
cp doxgrep.desktop "$APPDIR/usr/share/applications/"
cp doxgrep.desktop "$APPDIR/"

# Copy icon (if exists)
if [ -f "assets/doxgrep.png" ]; then
    cp assets/doxgrep.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/"
    cp assets/doxgrep.png "$APPDIR/doxgrep.png"
else
    echo "Warning: assets/doxgrep.png missing. Creating a dummy file so appimagetool can proceed."
    touch "$APPDIR/doxgrep.png"
fi

# Create AppRun (symlink to the binary)
ln -sf usr/bin/doxgrep "$APPDIR/AppRun"

# 3. Build AppImage
echo "--- Building AppImage ---"
# We use --appimage-extract-and-run to avoid FUSE issues on modern distros
# and export APPIMAGE_EXTRACT_AND_RUN=1 for the tool itself if it's an AppImage
export APPIMAGE_EXTRACT_AND_RUN=1
ARCH=x86_64 $APP_TOOL "$APPDIR" DoXgrep-x86_64.AppImage

echo "--- Done: DoXgrep-x86_64.AppImage ---"
