# DoXsearch

A blazingly fast, multi-threaded desktop document search tool built in **Rust** with a sleek GUI powered by **`egui`**.

DoXsearch allows you to instantly search for text inside **DOCX**, **ODT**, and **PDF** documents across entire directory trees.

---

## Key Features

- ⚡ **Multi-Threaded Parallel Search Engine** — Powered by `rayon` to utilize all CPU cores for lightning-fast text extraction and scanning.
- 📄 **Multi-Format Support** — Deep text extraction from Microsoft Word (`.docx`), OpenDocument Text (`.odt`), and Adobe PDF (`.pdf`) files.
- 🧠 **Memory-Efficient Case Matching** — Optimized zero-allocation case-insensitive search algorithm designed to prevent memory thrashing on large document stores.
- 📅 **Date & Relevance Sorting** — Sort matching files by Modification Date (newest/oldest first), File Name (A-Z), or Match Count.
- 📊 **Smooth Real-Time Progress Bar** — Non-flickering animated progress bar with live completion percentage and processed file counters.
- 🎯 **Highlighted Context Preview** — Displays matched keywords in surrounding text context with automatic line wrapping.
- 📂 **One-Click File Access** — Open any matched document directly in your operating system's default viewer.
- 🖥️ **Cross-Platform Bundles** — Standalone AppImage for Linux and Universal 2 `.app` bundle & `.dmg` installer for macOS (supporting both Apple Silicon M1/M2/M3/M4 and Intel Macs).

---

## Installation & Downloads

### macOS (Universal DMG / App Bundle)

Download the latest `.dmg` or `.zip` release from [GitHub Releases](https://github.com/vonfast/doc_searcher/releases):

1. Download **`DoXsearch-macOS-Universal.dmg`**.
2. Double-click the `.dmg` file and drag **DoXsearch.app** into your **Applications** folder.

> **Note for macOS users:**  
> Since the app is built without a paid Apple Developer certificate, macOS Gatekeeper may show a security notice on first launch (*"DoXsearch.app is from an unidentified developer"*).  
> **To open:** Right-click (or `Control` + click) **DoXsearch.app** in Finder, select **Open**, and confirm **Open**.  
> Alternatively, run `xattr -cr /Applications/DoXsearch.app` in Terminal to clear the download quarantine attribute.

---

### Linux (AppImage)

Download the standalone **AppImage** from [GitHub Releases](https://github.com/vonfast/doc_searcher/releases):

```bash
wget https://github.com/vonfast/doc_searcher/releases/latest/download/DoXsearch-x86_64.AppImage
chmod +x DoXsearch-x86_64.AppImage
./DoXsearch-x86_64.AppImage
```

---

## Build from Source

### Prerequisites

Ensure you have the Rust toolchain installed (`rustup`).

#### Linux Build Dependencies (Ubuntu/Debian)
```bash
sudo apt-get update
sudo apt-get install -y \
    libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev \
    libgtk-3-dev pkg-config libfontconfig1-dev zenity
```

#### Build and Run

```bash
# Clone repository
git clone https://github.com/vonfast/doc_searcher.git
cd doc_searcher

# Build and run release binary
cargo run --release
```

---

## Creating Packaging Bundles

### Building Linux AppImage
```bash
./build_appimage.sh
```

### Building macOS App Bundle & DMG
```bash
./build_mac_app.sh
```

---

## Tech Stack

- **Language:** Rust (2021 Edition)
- **GUI Framework:** [`eframe`](https://crates.io/crates/eframe) / [`egui`](https://crates.io/crates/egui) (v0.27)
- **Parallel Processing:** [`rayon`](https://crates.io/crates/rayon)
- **Document Parsing:** [`pdf-extract`](https://crates.io/crates/pdf-extract), [`quick-xml`](https://crates.io/crates/quick-xml), [`zip`](https://crates.io/crates/zip)
- **Date Formatting:** [`chrono`](https://crates.io/crates/chrono)
- **File Dialogs:** [`rfd`](https://crates.io/crates/rfd)

---

## License

Distributed under the [MIT License](LICENSE).
