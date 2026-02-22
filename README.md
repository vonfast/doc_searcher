# DoXgrep

A fast, lightweight desktop application for searching text within document files. Built with Rust and featuring a modern GUI powered by `egui`.

## Features

- **Multi-format Support** — Search through DOCX, ODT, and PDF files
- **Recursive Directory Search** — Scan entire folder trees with one click
- **Smart Search** — Case-sensitive/insensitive matching with adjustable context preview
- **Fast & Responsive** — Background search keeps the UI smooth
- **File Type Filtering** — Toggle specific formats on/off
- **Quick Access** — Open matched files directly from results
- **Native Integration** — GTK3 file picker on Linux

## Quick Start

### Download AppImage (Recommended)
Download the latest **AppImage** from [GitHub Releases](https://github.com/vonfast/doc_searcher/releases):

```bash
wget https://github.com/vonfast/doc_searcher/releases/latest/download/DoXgrep-x86_64.AppImage
chmod +x DoXgrep-x86_64.AppImage
./DoXgrep-x86_64.AppImage
```

### Build from Source

**Requirements:** Rust toolchain and system dependencies

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install dependencies (Fedora)
sudo dnf install gcc pkg-config libxcb-devel libxkbcommon-devel \
    wayland-devel mesa-libGL-devel fontconfig-devel zenity

# Build and run
cargo build --release
./target/release/doxgrep
```

## Building AppImage

Create a standalone AppImage package:

```bash
# Download appimagetool
wget https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage
chmod +x appimagetool-x86_64.AppImage

# Build AppImage
./build_appimage.sh
```

The resulting `DoXgrep-x86_64.AppImage` works on most Linux distributions without additional dependencies.

## Technology Stack

- **Language:** Rust 2021
- **GUI Framework:** egui/eframe
- **Document Parsing:** zip, pdf-extract, quick-xml
- **File Dialog:** rfd (GTK3 backend)

## License

MIT License or Apache 2.0 — choose whichever you prefer.
