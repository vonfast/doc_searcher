#!/bin/bash
set -e

# Tarkista appimagetool
if ! command -v appimagetool &> /dev/null && [ ! -f "./appimagetool-x86_64.AppImage" ]; then
    echo "Virhe: appimagetool puuttuu."
    echo "Lataa se: wget https://github.com/AppImage/AppImageKit/releases/download/13/appimagetool-x86_64.AppImage"
    echo "Tee suoritettavaksi: chmod +x appimagetool-x86_64.AppImage"
    exit 1
fi

APP_TOOL="./appimagetool-x86_64.AppImage"
if command -v appimagetool &> /dev/null; then
    APP_TOOL="appimagetool"
fi

# 1. Käännä release
echo "--- Käännetään release-build ---"
cargo build --release

# 2. Valmistele AppDir
APPDIR="target/doxgrep.AppDir"
echo "--- Valmistellaan AppDir: $APPDIR ---"
mkdir -p "$APPDIR/usr/bin"
mkdir -p "$APPDIR/usr/share/applications"
mkdir -p "$APPDIR/usr/share/icons/hicolor/256x256/apps"

# Kopioi binaari
cp target/release/doxgrep "$APPDIR/usr/bin/"

# Kopioi desktop-tiedosto
cp doxgrep.desktop "$APPDIR/usr/share/applications/"
cp doxgrep.desktop "$APPDIR/"

# Kopioi kuvake (jos olemassa)
if [ -f "assets/doxgrep.png" ]; then
    cp assets/doxgrep.png "$APPDIR/usr/share/icons/hicolor/256x256/apps/"
    cp assets/doxgrep.png "$APPDIR/doxgrep.png"
else
    echo "Varoitus: assets/doxgrep.png puuttuu. AppImage käyttää oletuskuvaketta."
fi

# Luo AppRun (jos puuttuu, yleensä appimagetool osaa linkittää binääriin jos desktop-tiedosto on juoressa)
# Mutta varmuuden vuoksi perinteinen linkitys:
ln -sf usr/bin/doxgrep "$APPDIR/AppRun"

# 3. Rakenna AppImage
echo "--- Rakennetaan AppImage ---"
ARCH=x86_64 $APP_TOOL "$APPDIR"

echo "--- Valmis: DoXgrep-x86_64.AppImage ---"
