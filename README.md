# DoXgrep

DoXgrep is a Rust-based desktop application for searching text within `.docx`, `.odt`, and `.pdf` files. It features a modern graphical user interface built with `egui`.

## Features

- **Multi-format Support**: Search through Microsoft Word (.docx), OpenDocument (.odt), and PDF files.
- **Recursive Search**: Search through all subdirectories within a chosen folder.
- **Case Sensitivity**: Toggle between case-sensitive and case-insensitive search.
- **Context Preview**: View snippets of text surrounding the matches, with adjustable context size.
- **Background Search**: Search operations run in a separate thread, keeping the UI responsive.
- **File Type Filtering**: Enable or disable specific file formats for your search.
- **Open Files**: Open found files directly with your system's default application.
- **Directory Browsing**: Easily select search directories using a native file picker (requires `zenity`).

## Installation (Linux)

### Download AppImage
The easiest way to use DoXgrep is to download the latest **AppImage** from the [GitHub Releases](https://github.com/vonfast/doc_searcher/releases) page.
1. Download `DoXgrep-x86_64.AppImage`.
2. Make it executable: `chmod +x DoXgrep-x86_64.AppImage`.
3. Run it: `./DoXgrep-x86_64.AppImage`.

### 1. Install Rust
```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source $HOME/.cargo/env
```

### 2. Install System Dependencies
On Fedora:
```bash
sudo dnf install gcc pkg-config \
    libxcb-devel libxkbcommon-devel \
    wayland-devel mesa-libGL-devel \
    fontconfig-devel zenity
```

### 3. Build and Run
```bash
cargo build --release
./target/release/doxgrep
```

## Distribution (AppImage)

You can build a standalone AppImage that works on most Linux distributions:

1. Download `appimagetool`:
   ```bash
   wget -O appimagetool-x86_64.AppImage https://github.com/AppImage/AppImageKit/releases/download/continuous/appimagetool-x86_64.AppImage
   chmod +x appimagetool-x86_64.AppImage
   ```
2. Run the build script:
   ```bash
   ./build_appimage.sh
   ```
3. The resulting package will be `DoXgrep-x86_64.AppImage`.

## Adding an Icon

To add a custom icon to the AppImage:
1. Place a PNG file (recommended size 256x256) at `assets/doxgrep.png`.
2. Re-run `./build_appimage.sh`.

## License

This project is licensed under the MIT License or Apache 2.0 (standard for Rust projects).
