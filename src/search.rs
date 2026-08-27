// src/search.rs - Search logic module

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct CacheKey {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

const MAX_CACHE_TOTAL_BYTES: usize = 64 * 1024 * 1024; // 64 MB
const MAX_CACHED_ENTRY_BYTES: usize = 1024 * 1024;     // 1 MB per file
const MAX_CACHE_ENTRIES: usize = 500;

#[derive(Clone, Default)]
pub struct DocumentCache {
    entries: Arc<RwLock<HashMap<CacheKey, Arc<str>>>>,
}

impl DocumentCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &CacheKey) -> Option<Arc<str>> {
        let guard = self.entries.read().ok()?;
        guard.get(key).cloned()
    }

    pub fn insert(&self, key: CacheKey, text: Arc<str>) {
        // Only cache documents up to 1 MB of text to prevent memory ballooning
        if text.len() > MAX_CACHED_ENTRY_BYTES {
            return;
        }
        if let Ok(mut guard) = self.entries.write() {
            let mut current_bytes: usize = guard
                .iter()
                .map(|(k, v)| std::mem::size_of::<CacheKey>() + k.path.as_os_str().len() + v.len())
                .sum();

            let entry_bytes = std::mem::size_of::<CacheKey>() + key.path.as_os_str().len() + text.len();

            // If exceeding max entries or total bytes, prune down
            if guard.len() >= MAX_CACHE_ENTRIES || current_bytes + entry_bytes > MAX_CACHE_TOTAL_BYTES {
                let target_bytes = MAX_CACHE_TOTAL_BYTES / 2;
                let keys_to_remove: Vec<CacheKey> = guard.keys().cloned().collect();
                for k in keys_to_remove {
                    if let Some(v) = guard.remove(&k) {
                        let rem_size = std::mem::size_of::<CacheKey>() + k.path.as_os_str().len() + v.len();
                        current_bytes = current_bytes.saturating_sub(rem_size);
                    }
                    if current_bytes <= target_bytes && guard.len() < (MAX_CACHE_ENTRIES / 2) {
                        break;
                    }
                }
            }
            guard.insert(key, text);
        }
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.write() {
            guard.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.entries.read().map(|g| g.len()).unwrap_or(0)
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn memory_usage_bytes(&self) -> usize {
        if let Ok(guard) = self.entries.read() {
            guard
                .iter()
                .map(|(k, v)| std::mem::size_of::<CacheKey>() + k.path.as_os_str().len() + v.len())
                .sum()
        } else {
            0
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: PathBuf,
    pub file_type: String,
    pub matches: Vec<Match>,
    pub modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone)]
pub struct SearchError {
    pub file: PathBuf,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct Match {
    pub context: String,
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub directory: PathBuf,
    pub query: String,
    pub ignore_case: bool,
    pub recursive: bool,
    pub search_hidden: bool,
    pub use_cache: bool,
    pub context_size: usize,
    pub search_docx: bool,
    pub search_odt: bool,
    pub search_pdf: bool,
    pub search_txt: bool,
    pub max_file_size_mb: Option<u64>,
    pub modified_after: Option<std::time::SystemTime>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("."),
            query: String::new(),
            ignore_case: false,
            recursive: true,
            search_hidden: false,
            use_cache: true,
            context_size: 150,
            search_docx: true,
            search_odt: true,
            search_pdf: true,
            search_txt: true,
            max_file_size_mb: None,
            modified_after: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchStats {
    pub cached_count: usize,
    pub total_count: usize,
    pub duration: std::time::Duration,
}

#[derive(Debug)]
struct SearchCandidate {
    path: PathBuf,
    ext: String,
    size: u64,
    modified: Option<std::time::SystemTime>,
}

pub fn search_directory(
    opts: &SearchOptions,
    cache: &DocumentCache,
    is_cancelled: Option<&std::sync::atomic::AtomicBool>,
    on_match: impl Fn(SearchResult) + Sync + Send,
    on_error: impl Fn(SearchError) + Sync + Send,
    progress_cb: impl Fn(usize, usize) + Sync + Send,
) -> Result<SearchStats> {
    let start_time = std::time::Instant::now();
    let max_depth = if opts.recursive { usize::MAX } else { 1 };
    let canonical_root = opts.directory.canonicalize().unwrap_or_else(|_| opts.directory.clone());

    let entries: Vec<SearchCandidate> = WalkDir::new(&canonical_root)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| {
            if let Some(cancel) = is_cancelled {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return false;
                }
            }
            if e.depth() == 0 {
                return true;
            }
            let is_dir = e.file_type().is_dir() || (e.file_type().is_symlink() && e.path().is_dir());
            let is_file = e.file_type().is_file() || (e.file_type().is_symlink() && e.path().is_file());
            let name = e.file_name().to_string_lossy();

            if !opts.search_hidden && name.starts_with('.') {
                return false;
            }

            if is_dir {
                if name == "node_modules"
                    || name == "target"
                    || name == "__pycache__"
                    || name == "venv"
                    || name == ".venv"
                    || name == ".cargo"
                    || name == ".rustup"
                    || name == ".local"
                    || name == ".cache"
                    || name == ".var"
                    || name == ".mozilla"
                    || name == ".steam"
                    || name == ".wine"
                    || name == ".flatpak"
                    || name == "build"
                {
                    return false;
                }
            } else if is_file {
                let ext = e.path()
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .unwrap_or("")
                    .to_lowercase();

                let matches_type = match ext.as_str() {
                    "docx" | "docm" | "dotx" | "dotm" => opts.search_docx,
                    "odt" | "ott" | "ods" | "odp" | "fodt" | "fods" => opts.search_odt,
                    "pdf" => opts.search_pdf,
                    "txt" | "text" | "md" | "markdown" | "csv" | "tsv" | "log" | "json" | "xml" | "html" | "htm" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "toml" | "rst" => opts.search_txt,
                    _ => false,
                };

                if !matches_type {
                    return false;
                }
            }
            true
        })
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            if !e.file_type().is_file() && !e.path().is_file() {
                return None;
            }
            let path = e.path();
            let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let ext = canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            let meta = e.metadata().or_else(|_| std::fs::metadata(path)).ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.and_then(|m| m.modified().ok());

            if let Some(max_mb) = opts.max_file_size_mb {
                let max_bytes = max_mb.saturating_mul(1024 * 1024);
                if size > max_bytes {
                    return None;
                }
            }

            if let Some(after) = opts.modified_after {
                match modified {
                    Some(mtime) if mtime >= after => {}
                    _ => return None,
                }
            }

            Some(SearchCandidate {
                path: canonical_path,
                ext,
                size,
                modified,
            })
        })
        .collect();

    let total = entries.len();
    progress_cb(0, total);
    if total == 0 {
        return Ok(SearchStats {
            cached_count: 0,
            total_count: 0,
            duration: start_time.elapsed(),
        });
    }

    let processed = std::sync::atomic::AtomicUsize::new(0);
    let cached_hits = std::sync::atomic::AtomicUsize::new(0);
    let step = if total <= 100 { 1 } else { (total / 100).max(1) };

    entries
        .into_par_iter()
        .for_each(|candidate| {
            if let Some(cancel) = is_cancelled {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
            }

            let cache_key = CacheKey {
                path: candidate.path.clone(),
                size: candidate.size,
                modified: candidate.modified,
            };

            let text_result: Result<Arc<str>> = if opts.use_cache {
                if let Some(cached) = cache.get(&cache_key) {
                    cached_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(cached)
                } else {
                    let raw_res = match candidate.ext.as_str() {
                        "fodt" | "fods" => extract_flat_xml(&candidate.path),
                        "docx" | "docm" | "dotx" | "dotm" => extract_docx(&candidate.path),
                        "odt" | "ott" | "ods" | "odp" => extract_odt(&candidate.path),
                        "pdf" => extract_pdf(&candidate.path),
                        "txt" | "text" | "md" | "markdown" | "csv" | "tsv" | "log" | "json" | "xml" | "html" | "htm" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "toml" | "rst" => extract_plain_text(&candidate.path),
                        _ => return,
                    };
                    match raw_res {
                        Ok(text) => {
                            let arc_text: Arc<str> = text.into();
                            cache.insert(cache_key, arc_text.clone());
                            Ok(arc_text)
                        }
                        Err(e) => Err(e),
                    }
                }
            } else {
                match candidate.ext.as_str() {
                    "fodt" | "fods" => extract_flat_xml(&candidate.path).map(|t| t.into()),
                    "docx" | "docm" | "dotx" | "dotm" => extract_docx(&candidate.path).map(|t| t.into()),
                    "odt" | "ott" | "ods" | "odp" => extract_odt(&candidate.path).map(|t| t.into()),
                    "pdf" => extract_pdf(&candidate.path).map(|t| t.into()),
                    "txt" | "text" | "md" | "markdown" | "csv" | "tsv" | "log" | "json" | "xml" | "html" | "htm" | "yaml" | "yml" | "ini" | "conf" | "cfg" | "toml" | "rst" => extract_plain_text(&candidate.path).map(|t| t.into()),
                    _ => return,
                }
            };

            let current = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if current.is_multiple_of(step) || current == total {
                progress_cb(current, total);
            }

            match text_result {
                Ok(text) => {
                    let matches = find_matches(&text, &opts.query, opts.ignore_case, opts.context_size);
                    if !matches.is_empty() {
                        let display_type = candidate.ext.to_uppercase();
                        on_match(SearchResult {
                            file: candidate.path,
                            file_type: display_type,
                            matches,
                            modified: candidate.modified,
                        });
                    }
                }
                Err(e) => {
                    on_error(SearchError {
                        file: candidate.path,
                        error: e.to_string(),
                    });
                }
            }
        });

    Ok(SearchStats {
        cached_count: cached_hits.load(std::sync::atomic::Ordering::Relaxed),
        total_count: total,
        duration: start_time.elapsed(),
    })
}

const MAX_PLAIN_TEXT_FILE_SIZE: u64 = 25 * 1024 * 1024; // 25 MB max

/// Extract text from plain text files (.txt, .md, .csv, .log, .json)
pub fn extract_plain_text(path: &Path) -> Result<String> {
    let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if file_len > MAX_PLAIN_TEXT_FILE_SIZE {
        return Err(anyhow::anyhow!(
            "Tekstitiedosto on liian suuri (>25 MB): {}",
            path.display()
        ));
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Could not read file: {}", path.display()))?;

    if bytes.is_empty() {
        return Ok(String::new());
    }

    // Binary check: inspect first 4096 bytes
    let sample = &bytes[..bytes.len().min(4096)];
    let null_count = sample.iter().filter(|&&b| b == 0).count();
    if null_count > 0 {
        return Err(anyhow::anyhow!(
            "Tiedosto sisältää binääridataa (nollatavuja): {}",
            path.display()
        ));
    }

    // Check control character density
    let control_count = sample
        .iter()
        .filter(|&&b| b < 0x20 && b != b'\t' && b != b'\n' && b != b'\r')
        .count();
    if sample.len() > 32 && control_count * 10 > sample.len() {
        return Err(anyhow::anyhow!(
            "Tiedosto näyttää olevan binääritiedosto: {}",
            path.display()
        ));
    }

    // 1. Try UTF-8 first
    let raw_text = if let Ok(s) = std::str::from_utf8(&bytes) {
        s.to_string()
    } else {
        // 2. If not UTF-8, decode as ISO-8859-1 (Latin-1) where bytes 0x00..=0xFF map 1:1 to Unicode U+0000..=U+00FF.
        // This preserves Nordic characters (ä, ö, å, Ä, Ö, Å) in legacy text/csv files.
        bytes.iter().map(|&b| b as char).collect()
    };

    // Sanitize control characters (except tab and newline) so egui font rendering won't corrupt
    let sanitized: String = raw_text
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' && c != '\r' {
                ' '
            } else {
                c
            }
        })
        .collect();

    Ok(sanitized)
}

/// Extract text from a .docx file using streaming XML reader across all body, header, footer, footnote, endnote, and comment parts
pub fn extract_docx(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    // Collect all XML parts: word/document.xml, word/header*.xml, word/footer*.xml, word/footnotes.xml, word/endnotes.xml, word/comments.xml
    let mut xml_names: Vec<String> = archive
        .file_names()
        .filter(|name| {
            let n = name.replace('\\', "/").to_lowercase();
            n.starts_with("word/") && n.ends_with(".xml") && (
                n == "word/document.xml"
                || n.starts_with("word/header")
                || n.starts_with("word/footer")
                || n.starts_with("word/footnotes")
                || n.starts_with("word/endnotes")
                || n.starts_with("word/comments")
            )
        })
        .map(|s| s.to_string())
        .collect();

    if xml_names.is_empty() {
        return Err(anyhow::anyhow!("word/document.xml missing from docx: {}", path.display()));
    }

    // Ensure word/document.xml comes first
    xml_names.sort_by(|a, b| {
        let a_main = a.ends_with("document.xml");
        let b_main = b.ends_with("document.xml");
        match (a_main, b_main) {
            (true, true) => a.cmp(b),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => a.cmp(b),
        }
    });

    let mut full_text = String::with_capacity(4096);
    for xml_name in xml_names {
        let entry = archive
            .by_name(&xml_name)
            .with_context(|| format!("Could not read part {xml_name} in docx: {}", path.display()))?;
        let reader = Reader::from_reader(std::io::BufReader::new(entry));
        let part_text = extract_text_from_xml_reader(reader, true)
            .with_context(|| format!("Error parsing XML in {xml_name} in docx: {}", path.display()))?;
        if !part_text.trim().is_empty() {
            if !full_text.is_empty() && !full_text.ends_with('\n') {
                full_text.push('\n');
            }
            full_text.push_str(&part_text);
        }
    }

    Ok(full_text)
}

/// Extract text from a flat OpenDocument XML file (.fodt, .fods)
pub fn extract_flat_xml(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let reader = Reader::from_reader(std::io::BufReader::new(file));
    extract_text_from_xml_reader(reader, false)
        .with_context(|| format!("Error reading XML in: {}", path.display()))
}

/// Extract text from a .odt file using streaming XML reader across content.xml and styles.xml
pub fn extract_odt(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let mut full_text = String::with_capacity(4096);

    // 1. content.xml (main document body)
    let doc = archive
        .by_name("content.xml")
        .with_context(|| format!("content.xml missing from odt: {}", path.display()))?;
    let reader = Reader::from_reader(std::io::BufReader::new(doc));
    let body_text = extract_text_from_xml_reader(reader, false)
        .with_context(|| format!("Error reading content.xml in odt: {}", path.display()))?;
    full_text.push_str(&body_text);

    // 2. styles.xml (contains headers and footers in ODT)
    if let Ok(styles) = archive.by_name("styles.xml") {
        let reader = Reader::from_reader(std::io::BufReader::new(styles));
        let style_text = extract_text_from_xml_reader(reader, false)
            .with_context(|| format!("Error reading styles.xml in odt: {}", path.display()))?;
        if !style_text.trim().is_empty() {
            if !full_text.is_empty() && !full_text.ends_with('\n') {
                full_text.push('\n');
            }
            full_text.push_str(&style_text);
        }
    }

    Ok(full_text)
}

pub fn extract_text_from_xml_reader<R: std::io::BufRead>(
    mut reader: Reader<R>,
    is_docx: bool,
) -> Result<String> {
    reader.trim_text(false);
    reader.check_end_names(true);

    let mut text_content = String::with_capacity(4096);
    let mut buf = Vec::new();

    let mut in_text_node = false;
    let mut in_paragraph = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let local = e.local_name();
                let local_bytes = local.as_ref();
                if local_bytes == b"t" || local_bytes == b"delText" {
                    in_text_node = true;
                }
                if local_bytes == b"p" || local_bytes == b"h" {
                    in_paragraph = true;
                }
            }
            Ok(Event::Text(e)) => {
                let should_extract = if is_docx {
                    in_text_node
                } else {
                    in_text_node || in_paragraph
                };

                if should_extract {
                    let text = e.unescape().unwrap_or_default();
                    text_content.push_str(&text);
                }
            }
            Ok(Event::CData(e)) => {
                let should_extract = if is_docx {
                    in_text_node
                } else {
                    in_text_node || in_paragraph
                };

                if should_extract {
                    if let Ok(text) = std::str::from_utf8(e.as_ref()) {
                        text_content.push_str(text);
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let local = e.local_name();
                match local.as_ref() {
                    b"tab" => {
                        if in_paragraph || in_text_node {
                            text_content.push('\t');
                        }
                    }
                    b"br" | b"cr" | b"line-break" => {
                        if in_paragraph || in_text_node {
                            text_content.push('\n');
                        }
                    }
                    b"s" => {
                        if in_paragraph || in_text_node {
                            let count = e
                                .attributes()
                                .filter_map(|a| a.ok())
                                .find(|a| a.key.local_name().as_ref() == b"c")
                                .and_then(|a| std::str::from_utf8(&a.value).ok()?.parse::<usize>().ok())
                                .unwrap_or(1);
                            for _ in 0..count {
                                text_content.push(' ');
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let local = e.local_name();
                let local_bytes = local.as_ref();
                if local_bytes == b"t" || local_bytes == b"delText" {
                    in_text_node = false;
                }
                if local_bytes == b"p" || local_bytes == b"h" {
                    in_paragraph = false;
                    if !text_content.ends_with('\n') && !text_content.is_empty() {
                        text_content.push('\n');
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(anyhow::anyhow!("XML parse error: {e}")),
            _ => {}
        }
        buf.clear();
    }

    Ok(text_content)
}

#[allow(dead_code)]
pub fn extract_text_from_xml(xml: &str) -> Result<String> {
    let is_docx = xml.contains("w:document") || xml.contains("w:p") || xml.contains("w:t");
    let reader = Reader::from_str(xml);
    extract_text_from_xml_reader(reader, is_docx)
}

/// Extract text from a .pdf file with header sanitization, panic safety, and fallback stream parsing
pub fn extract_pdf(path: &Path) -> Result<String> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 100 * 1024 * 1024 {
        return Err(anyhow::anyhow!("PDF-tiedosto on liian suuri (>100 MB): {}", path.display()));
    }

    let bytes = std::fs::read(path)
        .with_context(|| format!("Could not read file: {}", path.display()))?;

    if bytes.is_empty() {
        return Err(anyhow::anyhow!("Tiedosto on tyhjä (0 tavua)"));
    }

    // 1. Sanitize header: if there are leading bytes before %PDF-, trim to %PDF-
    let pdf_bytes = if let Some(pos) = bytes.windows(5).position(|w| w == b"%PDF-") {
        if pos > 0 {
            &bytes[pos..]
        } else {
            &bytes[..]
        }
    } else {
        // If %PDF- header is missing, try raw stream fallback extraction
        if let Some(text) = extract_pdf_fallback(&bytes) {
            return Ok(text);
        }
        return Err(anyhow::anyhow!("Tiedosto ei ole kelvollinen PDF (puuttuva %PDF-otsake)"));
    };

    // 2. Try standard pdf-extract in-memory directly from the byte slice
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(pdf_bytes)
    }));

    match res {
        Ok(Ok(text)) if !text.trim().is_empty() => Ok(text),
        _ => {
            // 3. Fallback stream parser if pdf-extract failed on corrupted cross-reference table or fonts
            if let Some(text) = extract_pdf_fallback(pdf_bytes) {
                Ok(text)
            } else {
                Err(anyhow::anyhow!("PDF-tiedoston tekstirakenne on vioittunut tai suojattu"))
            }
        }
    }
}

/// Fallback parser that scans raw PDF streams when xref table or headers are malformed
fn extract_pdf_fallback(bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut extracted_text = String::new();
    let mut i = 0;
    let len = bytes.len();
    let mut zlib_buf = Vec::new();
    let mut deflate_buf = Vec::new();

    while i < len {
        if let Some(stream_pos) = bytes[i..].windows(6).position(|w| w == b"stream") {
            let start = i + stream_pos + 6;
            let mut stream_start = start;
            if stream_start < len && bytes[stream_start] == b'\r' {
                stream_start += 1;
            }
            if stream_start < len && bytes[stream_start] == b'\n' {
                stream_start += 1;
            }

            if let Some(end_pos) = bytes[stream_start..].windows(9).position(|w| w == b"endstream") {
                let stream_bytes = &bytes[stream_start..stream_start + end_pos];

                // Try decompressing as zlib/flate reusing pre-allocated buffers
                zlib_buf.clear();
                let mut decoder = flate2::read::ZlibDecoder::new(stream_bytes);
                let decompressed = if decoder.read_to_end(&mut zlib_buf).is_ok() && !zlib_buf.is_empty() {
                    Some(&zlib_buf[..])
                } else {
                    deflate_buf.clear();
                    let mut decoder2 = flate2::read::DeflateDecoder::new(stream_bytes);
                    if decoder2.read_to_end(&mut deflate_buf).is_ok() && !deflate_buf.is_empty() {
                        Some(&deflate_buf[..])
                    } else {
                        None
                    }
                };

                let mut stream_extracted = String::new();
                if let Some(dec) = decompressed {
                    extract_text_from_pdf_stream(dec, &mut stream_extracted);
                }
                if stream_extracted.trim().is_empty() {
                    extract_text_from_pdf_stream(stream_bytes, &mut stream_extracted);
                }
                if !stream_extracted.trim().is_empty() {
                    extracted_text.push_str(&stream_extracted);
                    extracted_text.push(' ');
                }

                i = stream_start + end_pos + 9;
            } else {
                break;
            }
        } else {
            break;
        }
    }

    if extracted_text.trim().is_empty() {
        None
    } else {
        Some(extracted_text)
    }
}

/// Extract printable strings from a PDF content stream (BT ... ET blocks, (text) Tj, [(t)(e)(x)(t)] TJ, <hex> Tj)
fn extract_text_from_pdf_stream(stream: &[u8], out: &mut String) {
    let mut in_paren = false;
    let mut in_hex = false;
    let mut escaped = false;
    let mut current_str: Vec<u8> = Vec::new();
    let mut hex_str: Vec<u8> = Vec::new();

    for &b in stream {
        if in_paren {
            if escaped {
                match b {
                    b'n' => current_str.push(b'\n'),
                    b'r' => current_str.push(b'\r'),
                    b't' => current_str.push(b'\t'),
                    b'\\' => current_str.push(b'\\'),
                    b'(' => current_str.push(b'('),
                    b')' => current_str.push(b')'),
                    _ => current_str.push(b),
                }
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b')' {
                in_paren = false;
                if let Ok(s) = std::str::from_utf8(&current_str) {
                    if !s.trim().is_empty() {
                        out.push_str(s);
                        out.push(' ');
                    }
                } else {
                    let latin1: String = current_str.iter().map(|&c| c as char).collect();
                    if !latin1.trim().is_empty() {
                        out.push_str(&latin1);
                        out.push(' ');
                    }
                }
                current_str.clear();
            } else {
                current_str.push(b);
            }
        } else if in_hex {
            if b == b'>' {
                in_hex = false;
                let hex_clean: Vec<u8> = hex_str.iter().copied().filter(|c| !c.is_ascii_whitespace()).collect();
                let mut decoded = Vec::with_capacity(hex_clean.len() / 2);
                let mut chunk = hex_clean.chunks_exact(2);
                for pair in &mut chunk {
                    if let (Ok(h1), Ok(h2)) = (std::str::from_utf8(&pair[0..1]), std::str::from_utf8(&pair[1..2])) {
                        if let Ok(val) = u8::from_str_radix(&format!("{}{}", h1, h2), 16) {
                            if val >= 32 || val == b'\n' || val == b'\r' || val == b'\t' {
                                decoded.push(val);
                            }
                        }
                    }
                }
                if let Ok(s) = std::str::from_utf8(&decoded) {
                    if !s.trim().is_empty() {
                        out.push_str(s);
                        out.push(' ');
                    }
                } else {
                    let latin1: String = decoded.iter().map(|&c| c as char).collect();
                    if !latin1.trim().is_empty() {
                        out.push_str(&latin1);
                        out.push(' ');
                    }
                }
                hex_str.clear();
            } else if b.is_ascii_hexdigit() || b.is_ascii_whitespace() {
                hex_str.push(b);
            }
        } else if b == b'(' {
            in_paren = true;
            escaped = false;
            current_str.clear();
        } else if b == b'<' {
            in_hex = true;
            hex_str.clear();
        }
    }
}

pub fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

pub fn starts_with_ignore_case(text_slice: &str, query: &str) -> Option<usize> {
    let mut text_chars = text_slice.chars().peekable();
    let mut query_chars = query.chars().peekable();
    let mut bytes_read = 0;

    while let Some(&q_char) = query_chars.peek() {
        if q_char.is_whitespace() {
            while let Some(&qc) = query_chars.peek() {
                if qc.is_whitespace() {
                    query_chars.next();
                } else {
                    break;
                }
            }

            let mut matched_space = false;
            while let Some(&tc) = text_chars.peek() {
                if tc.is_whitespace() {
                    matched_space = true;
                    bytes_read += tc.len_utf8();
                    text_chars.next();
                } else {
                    break;
                }
            }
            if !matched_space {
                return None;
            }
        } else {
            query_chars.next();
            if let Some(t_char) = text_chars.next() {
                bytes_read += t_char.len_utf8();
                let mut lowered_t = t_char.to_lowercase();
                let mut lowered_q = q_char.to_lowercase();
                let first_t = lowered_t.next();
                let first_q = lowered_q.next();
                if first_t != first_q {
                    return None;
                }
                for (ct, cq) in lowered_t.by_ref().zip(lowered_q.by_ref()) {
                    if ct != cq {
                        return None;
                    }
                }
                if lowered_t.next().is_some() || lowered_q.next().is_some() {
                    return None;
                }
            } else {
                return None;
            }
        }
    }
    Some(bytes_read)
}

pub fn starts_with_exact(text_slice: &str, query: &str) -> Option<usize> {
    let mut text_chars = text_slice.chars().peekable();
    let mut query_chars = query.chars().peekable();
    let mut bytes_read = 0;

    while let Some(&q_char) = query_chars.peek() {
        if q_char.is_whitespace() {
            while let Some(&qc) = query_chars.peek() {
                if qc.is_whitespace() {
                    query_chars.next();
                } else {
                    break;
                }
            }

            let mut matched_space = false;
            while let Some(&tc) = text_chars.peek() {
                if tc.is_whitespace() {
                    matched_space = true;
                    bytes_read += tc.len_utf8();
                    text_chars.next();
                } else {
                    break;
                }
            }
            if !matched_space {
                return None;
            }
        } else {
            query_chars.next();
            if let Some(t_char) = text_chars.next() {
                bytes_read += t_char.len_utf8();
                if t_char != q_char {
                    return None;
                }
            } else {
                return None;
            }
        }
    }
    Some(bytes_read)
}

pub const MAX_MATCHES_PER_FILE: usize = 200;

pub fn find_match_spans(
    text: &str,
    query: &str,
    ignore_case: bool,
) -> Vec<(usize, usize)> {
    find_match_spans_limit(text, query, ignore_case, None)
}

pub fn find_match_spans_limit(
    text: &str,
    query: &str,
    ignore_case: bool,
    limit: Option<usize>,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let trimmed_query = query.trim();
    if trimmed_query.is_empty() {
        return spans;
    }

    let mut char_indices = text.char_indices().peekable();

    while let Some(&(start_idx, _)) = char_indices.peek() {
        if let Some(max) = limit {
            if spans.len() >= max {
                break;
            }
        }

        let match_bytes = if ignore_case {
            starts_with_ignore_case(&text[start_idx..], trimmed_query)
        } else {
            starts_with_exact(&text[start_idx..], trimmed_query)
        };

        if let Some(bytes) = match_bytes {
            if bytes == 0 {
                char_indices.next();
                continue;
            }
            let end_idx = start_idx + bytes;
            spans.push((start_idx, end_idx));

            // Move past the match
            while let Some(&(i, _)) = char_indices.peek() {
                if i < end_idx {
                    char_indices.next();
                } else {
                    break;
                }
            }
        } else {
            char_indices.next();
        }
    }

    spans
}

pub fn find_matches(
    text: &str,
    query: &str,
    ignore_case: bool,
    context_size: usize,
) -> Vec<Match> {
    find_matches_limit(text, query, ignore_case, context_size, Some(MAX_MATCHES_PER_FILE))
}

pub fn find_matches_limit(
    text: &str,
    query: &str,
    ignore_case: bool,
    context_size: usize,
    limit: Option<usize>,
) -> Vec<Match> {
    let spans = find_match_spans_limit(text, query, ignore_case, limit);
    let mut matches = Vec::with_capacity(spans.len());

    for (start_idx, end_idx) in spans {
        let ctx_start_raw = start_idx.saturating_sub(context_size);
        let ctx_end_raw = (end_idx + context_size).min(text.len());

        let actual_start = floor_char_boundary(text, ctx_start_raw);
        let actual_end = ceil_char_boundary(text, ctx_end_raw);

        let sub_slice = &text[actual_start..actual_end];
        let mut context = String::with_capacity(sub_slice.len() + 8);
        if actual_start > 0 {
            context.push_str("… ");
        }
        let mut prev_space = false;
        for c in sub_slice.chars() {
            if c.is_whitespace() {
                if !prev_space {
                    context.push(' ');
                    prev_space = true;
                }
            } else {
                context.push(c);
                prev_space = false;
            }
        }
        if actual_end < text.len() {
            context.push_str(" …");
        }

        matches.push(Match { context });
    }

    matches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multibyte_search() {
        let text = "Äiti on kiva";
        let query = "Ä";
        let matches = find_matches(text, query, false, 10);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_case_insensitive_multibyte_lowercase_size_change() {
        let text = "GROẞE";
        let query = "große";
        let matches = find_matches(text, query, true, 10);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_overlap_avoidance() {
        let text = "aaaaa";
        let query = "aa";
        let matches = find_matches(text, query, false, 10);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_boundary_panic_prevention() {
        let text = "🦀🦀🦀";
        let query = "🦀";
        let matches = find_matches(text, query, false, 10);
        assert_eq!(matches.len(), 3);
    }

    #[test]
    fn test_xml_multi_run_phrase_matching() {
        let xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
            <w:body>
                <w:p>
                    <w:r><w:t>Tämä on </w:t></w:r>
                    <w:r><w:rPr><w:b/></w:rPr><w:t>tärkeä</w:t></w:r>
                    <w:r><w:t> asiakirja.</w:t></w:r>
                </w:p>
                <w:p>
                    <w:r><w:t>Toinen kappale.</w:t></w:r>
                </w:p>
            </w:body>
        </w:document>"#;
        let extracted = extract_text_from_xml(xml).expect("XML extraction failed");
        assert_eq!(extracted, "Tämä on tärkeä asiakirja.\nToinen kappale.\n");

        let matches = find_matches(&extracted, "tärkeä asiakirja", false, 50);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_odt_xml_extraction() {
        let xml = r#"<office:document-content xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
            <text:p>Ensimmäinen<text:s text:c="3"/>kohta</text:p>
            <text:h>Otsikko</text:h>
        </office:document-content>"#;
        let extracted = extract_text_from_xml(xml).expect("XML extraction failed");
        assert_eq!(extracted, "Ensimmäinen   kohta\nOtsikko\n");
    }

    #[test]
    fn test_find_match_spans_accuracy() {
        let text = "Öljy ja Vesi öljy";
        let spans = find_match_spans(text, "öljy", true);
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].0..spans[0].1], "Öljy");
        assert_eq!(&text[spans[1].0..spans[1].1], "öljy");
    }

    #[test]
    fn test_streaming_xml_reader() {
        let xml = "<w:document><w:body><w:p><w:r><w:t>Streaming XML Test</w:t></w:r></w:p></w:body></w:document>";
        let cursor = std::io::Cursor::new(xml.as_bytes());
        let reader = Reader::from_reader(cursor);
        let extracted = extract_text_from_xml_reader(reader, true).expect("Streaming XML extraction failed");
        assert_eq!(extracted, "Streaming XML Test\n");
    }

    #[test]
    fn test_search_options_default() {
        let opts = SearchOptions::default();
        assert!(!opts.search_hidden);
        assert!(opts.use_cache);
        assert!(opts.search_docx);
        assert!(opts.search_odt);
        assert!(opts.search_pdf);
        assert!(opts.search_txt);
        assert!(opts.max_file_size_mb.is_none());
        assert!(opts.modified_after.is_none());
    }

    #[test]
    fn test_extract_plain_text_from_tempfile() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("doxsearch_test_plain.txt");
        std::fs::write(&test_file, "Tämä on puhdasta tekstiä testiin.\nToinen rivi.").expect("write failed");

        let extracted = extract_plain_text(&test_file).expect("extract failed");
        assert!(extracted.contains("puhdasta tekstiä"));

        let _ = std::fs::remove_file(test_file);
    }

    #[test]
    fn test_cache_insert_and_get() {
        let cache = DocumentCache::new();
        let key = CacheKey {
            path: PathBuf::from("/tmp/test.docx"),
            size: 1024,
            modified: None,
        };
        assert!(cache.get(&key).is_none());
        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());

        let text: Arc<str> = "Tämä on testisisältöä".into();
        cache.insert(key.clone(), text.clone());

        assert_eq!(cache.len(), 1);
        assert!(!cache.is_empty());
        assert_eq!(cache.get(&key), Some(text));
    }

    #[test]
    fn test_cache_invalidation_on_modified_or_size() {
        let cache = DocumentCache::new();
        let key1 = CacheKey {
            path: PathBuf::from("/tmp/doc.docx"),
            size: 1000,
            modified: Some(std::time::UNIX_EPOCH),
        };
        cache.insert(key1.clone(), "Version 1".into());

        // Same path, different size
        let key2 = CacheKey {
            path: PathBuf::from("/tmp/doc.docx"),
            size: 1050,
            modified: Some(std::time::UNIX_EPOCH),
        };
        assert!(cache.get(&key2).is_none());

        // Same path, different modified time
        let key3 = CacheKey {
            path: PathBuf::from("/tmp/doc.docx"),
            size: 1000,
            modified: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(10)),
        };
        assert!(cache.get(&key3).is_none());
    }

    #[test]
    fn test_cache_clear_and_memory() {
        let cache = DocumentCache::new();
        let key = CacheKey {
            path: PathBuf::from("/tmp/doc.docx"),
            size: 500,
            modified: None,
        };
        cache.insert(key, "Content".into());
        assert!(cache.memory_usage_bytes() > 0);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.memory_usage_bytes(), 0);
    }

    #[test]
    fn test_pdf_stream_fallback_extraction() {
        // Create a raw mock PDF stream containing text in parentheses
        let mock_stream = b"BT /F1 12 Tf (Hello World) Tj ET (Verokortti 2025) Tj";
        let mut out = String::new();
        extract_text_from_pdf_stream(mock_stream, &mut out);
        assert!(out.contains("Hello World"));
        assert!(out.contains("Verokortti 2025"));
    }

    #[test]
    fn test_repeated_search_uses_cache() {
        let temp_dir = std::env::temp_dir().join("doxsearch_cache_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let test_file = temp_dir.join("sample.txt");
        std::fs::write(&test_file, "Tämä on välimuistitestitiedosto hakua varten.").unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "välimuistitesti".to_string(),
            use_cache: true,
            ..Default::default()
        };

        // First search: cache miss -> loads from file
        let stats1 = search_directory(&opts, &cache, None, |_| {}, |_| {}, |_, _| {}).unwrap();
        assert_eq!(stats1.total_count, 1);
        assert_eq!(stats1.cached_count, 0);
        assert_eq!(cache.len(), 1);

        // Second search with different query: cache hit -> 100% from cache!
        let opts2 = SearchOptions {
            directory: temp_dir.clone(),
            query: "toinen_haku".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats2 = search_directory(&opts2, &cache, None, |_| {}, |_| {}, |_, _| {}).unwrap();
        assert_eq!(stats2.total_count, 1);
        assert_eq!(stats2.cached_count, 1);
        // Cache size must NOT increase / double!
        assert_eq!(cache.len(), 1);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_extract_plain_text_latin1_fallback() {
        let temp_dir = std::env::temp_dir();
        let test_file = temp_dir.join("doxsearch_test_latin1.txt");
        // ISO-8859-1 encoded string: "Sähköposti ja Päivämäärä"
        // 'ä' = 0xE4, 'ö' = 0xF6, 'ä' = 0xE4, 'ä' = 0xE4
        let latin1_bytes: Vec<u8> = vec![
            b'S', 0xE4, b'h', b'k', 0xF6, b'p', b'o', b's', b't', b'i', b' ',
            b'j', b'a', b' ',
            b'P', 0xE4, b'i', b'v', 0xE4, b'm', 0xE4, 0xE4, b'r', 0xE4,
        ];
        std::fs::write(&test_file, &latin1_bytes).expect("write failed");

        let extracted = extract_plain_text(&test_file).expect("extract failed");
        assert_eq!(extracted, "Sähköposti ja Päivämäärä");

        let matches = find_matches(&extracted, "sähköposti", true, 20);
        assert_eq!(matches.len(), 1);

        let _ = std::fs::remove_file(test_file);
    }

    #[test]
    fn test_search_cancellation() {
        let temp_dir = std::env::temp_dir().join("doxsearch_cancel_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        for i in 0..5 {
            let file = temp_dir.join(format!("doc{}.txt", i));
            let _ = std::fs::write(&file, format!("Tiedosto numero {}", i));
        }

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "Tiedosto".to_string(),
            ..Default::default()
        };

        let cancel_flag = std::sync::atomic::AtomicBool::new(true); // cancelled from start
        let match_count = std::sync::atomic::AtomicUsize::new(0);

        let _ = search_directory(
            &opts,
            &cache,
            Some(&cancel_flag),
            |_| {
                match_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            },
            |_| {},
            |_, _| {},
        );

        assert_eq!(match_count.load(std::sync::atomic::Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_pdf_stream_hex_extraction() {
        let mock_stream = b"BT /F1 12 Tf <48656C6C6F20576F726C64> Tj ET";
        let mut out = String::new();
        extract_text_from_pdf_stream(mock_stream, &mut out);
        assert!(out.contains("Hello World"));
    }

    #[test]
    fn test_cache_partial_eviction_over_capacity() {
        let cache = DocumentCache::new();
        for i in 0..600 {
            let key = CacheKey {
                path: PathBuf::from(format!("/tmp/doc_{}.docx", i)),
                size: 100,
                modified: None,
            };
            cache.insert(key, format!("Content {}", i).into());
        }
        // Should have pruned elements to under MAX_CACHE_ENTRIES
        assert!(cache.len() >= 200);
        assert!(cache.len() <= MAX_CACHE_ENTRIES);
    }

    #[test]
    fn test_find_matches_max_limit_capping() {
        // Create a string with 1000 occurrences of "test"
        let text = "test ".repeat(1000);
        let matches = find_matches(&text, "test", false, 10);
        // Must be capped at MAX_MATCHES_PER_FILE (200)
        assert_eq!(matches.len(), MAX_MATCHES_PER_FILE);
    }

    #[test]
    fn test_binary_file_detection_in_plain_text() {
        let temp_dir = std::env::temp_dir();
        let bin_file = temp_dir.join("doxsearch_fake_text_with_nulls.txt");
        let mut binary_data = b"Hello world! This looks like text initially...".to_vec();
        binary_data.push(0x00); // null byte
        binary_data.extend_from_slice(b"Some binary trailer data");
        std::fs::write(&bin_file, &binary_data).expect("write failed");

        let result = extract_plain_text(&bin_file);
        assert!(result.is_err(), "Binary file containing null bytes must be rejected");

        let _ = std::fs::remove_file(bin_file);
    }

    #[test]
    fn test_cache_memory_budget_enforcement() {
        let cache = DocumentCache::new();
        // Insert items up to limit
        for i in 0..100 {
            let key = CacheKey {
                path: PathBuf::from(format!("/tmp/doc_budget_{}.txt", i)),
                size: 500_000,
                modified: None,
            };
            // 500 KB each * 100 = 50 MB
            let data: Arc<str> = "A".repeat(500_000).into();
            cache.insert(key, data);
        }
        assert!(cache.memory_usage_bytes() <= MAX_CACHE_TOTAL_BYTES);
    }

    #[test]
    fn test_xml_malformed_returns_error() {
        let malformed_xml = "<w:document><w:body><w:p><w:t>Hello</w:wrong></w:p></w:body></w:document>";
        let res = extract_text_from_xml(malformed_xml);
        assert!(res.is_err(), "Malformed XML should return an error");
    }

    #[test]
    fn test_max_file_size_overflow_safety() {
        let max_mb: u64 = u64::MAX;
        let max_bytes = max_mb.saturating_mul(1024 * 1024);
        assert_eq!(max_bytes, u64::MAX);
    }

    #[test]
    fn test_multi_format_directory_search_finds_all_types() {
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_multi_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // 1. Plain text file
        let txt_path = temp_dir.join("sample.txt");
        std::fs::write(&txt_path, "Tämä on hakutermi tekstissä").unwrap();

        // 2. Markdown file
        let md_path = temp_dir.join("notes.md");
        std::fs::write(&md_path, "Muistiinpanot: hakutermi löytyy täältä").unwrap();

        // 3. Mock PDF file with stream text
        let pdf_path = temp_dir.join("doc.pdf");
        let pdf_content = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (hakutermi pdf-tiedostossa) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_path, pdf_content).unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "hakutermi".to_string(),
            search_docx: true,
            search_odt: true,
            search_pdf: true,
            search_txt: true,
            ..Default::default()
        };

        let matches_found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_clone = matches_found.clone();

        let stats = search_directory(
            &opts,
            &cache,
            None,
            move |res| {
                m_clone.lock().unwrap().push(res);
            },
            |_| {},
            |_, _| {},
        ).unwrap();

        let results = matches_found.lock().unwrap();
        assert_eq!(results.len(), 3, "All 3 supported format files must be found");
        let types: Vec<String> = results.iter().map(|r| r.file_type.clone()).collect();
        assert!(types.contains(&"TXT".to_string()) || types.contains(&"MD".to_string()));
        assert!(types.contains(&"PDF".to_string()));
        assert_eq!(stats.total_count, 3);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_repeated_search_with_changing_file_type_options() {
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_repeat_opts_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let txt_path = temp_dir.join("sample.txt");
        std::fs::write(&txt_path, "UniikkiHakusana tekstissä").unwrap();

        let pdf_path = temp_dir.join("doc.pdf");
        let pdf_content = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (UniikkiHakusana pdf-tiedostossa) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_path, pdf_content).unwrap();

        let cache = DocumentCache::new();

        // 1st search: only TXT
        let opts1 = SearchOptions {
            directory: temp_dir.clone(),
            query: "UniikkiHakusana".to_string(),
            search_docx: false,
            search_odt: false,
            search_pdf: false,
            search_txt: true,
            ..Default::default()
        };
        let matches1 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m1 = matches1.clone();
        let _ = search_directory(&opts1, &cache, None, move |r| m1.lock().unwrap().push(r), |_| {}, |_, _| {}).unwrap();
        assert_eq!(matches1.lock().unwrap().len(), 1);
        assert_eq!(matches1.lock().unwrap()[0].file_type, "TXT");

        // 2nd search: only PDF
        let opts2 = SearchOptions {
            directory: temp_dir.clone(),
            query: "UniikkiHakusana".to_string(),
            search_docx: false,
            search_odt: false,
            search_pdf: true,
            search_txt: false,
            ..Default::default()
        };
        let matches2 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m2 = matches2.clone();
        let _ = search_directory(&opts2, &cache, None, move |r| m2.lock().unwrap().push(r), |_| {}, |_, _| {}).unwrap();
        assert_eq!(matches2.lock().unwrap().len(), 1);
        assert_eq!(matches2.lock().unwrap()[0].file_type, "PDF");

        // 3rd search: both TXT and PDF
        let opts3 = SearchOptions {
            directory: temp_dir.clone(),
            query: "UniikkiHakusana".to_string(),
            search_docx: true,
            search_odt: true,
            search_pdf: true,
            search_txt: true,
            ..Default::default()
        };
        let matches3 = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m3 = matches3.clone();
        let _ = search_directory(&opts3, &cache, None, move |r| m3.lock().unwrap().push(r), |_| {}, |_, _| {}).unwrap();
        assert_eq!(matches3.lock().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_extract_flat_xml_fodt() {
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_fodt_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let fodt_path = temp_dir.join("sample.fodt");
        let xml_content = r#"<?xml version="1.0" encoding="UTF-8"?>
        <office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                         xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
            <office:body>
                <office:text>
                    <text:p>Tämä on suora Flat ODF XML tiedosto.</text:p>
                </office:text>
            </office:body>
        </office:document>"#;
        std::fs::write(&fodt_path, xml_content).unwrap();

        let extracted = extract_flat_xml(&fodt_path).expect("Flat XML extract failed");
        assert!(extracted.contains("suora Flat ODF XML tiedosto"));

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_multiword_whitespace_and_newline_matching() {
        let text = "Tämä on tärkeä\nasiakirja jossa on   useita   välejä.";
        // Search with standard space
        let matches = find_matches(text, "tärkeä asiakirja", true, 30);
        assert_eq!(matches.len(), 1, "Should match across newline");

        // Search with multiple spaces in query
        let matches2 = find_matches(text, "useita       välejä", true, 30);
        assert_eq!(matches2.len(), 1, "Should match multiple spaces in query to multiple spaces in text");

        // Exact case search
        let matches3 = find_matches(text, "tärkeä asiakirja", false, 30);
        assert_eq!(matches3.len(), 1);
    }

    #[test]
    fn test_docx_sort_by_strict_weak_ordering() {
        let mut names = vec![
            "word/document.xml".to_string(),
            "word/document.xml".to_string(),
            "word/header1.xml".to_string(),
            "word/footer1.xml".to_string(),
        ];

        names.sort_by(|a, b| {
            let a_main = a.ends_with("document.xml");
            let b_main = b.ends_with("document.xml");
            match (a_main, b_main) {
                (true, true) => a.cmp(b),
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                (false, false) => a.cmp(b),
            }
        });

        assert_eq!(names[0], "word/document.xml");
        assert_eq!(names[1], "word/document.xml");
    }

    #[test]
    fn test_date_filter_excludes_none_and_respects_after() {
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_date_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let file1 = temp_dir.join("recent.txt");
        std::fs::write(&file1, "Tämä on tuore tiedosto").unwrap();

        let cache = DocumentCache::new();
        let now = std::time::SystemTime::now();

        // Filter requiring file modified in the future (no files should match)
        let future_time = now + std::time::Duration::from_secs(3600);
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "tuore".to_string(),
            modified_after: Some(future_time),
            ..Default::default()
        };

        let matches_found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_clone = matches_found.clone();
        let _ = search_directory(&opts, &cache, None, move |r| m_clone.lock().unwrap().push(r), |_| {}, |_, _| {}).unwrap();

        assert_eq!(matches_found.lock().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_directory_change_searches_all_supported_types() {
        let base_dir = std::env::temp_dir().join(format!("doxsearch_dir_switch_{}", std::process::id()));
        let dir_a = base_dir.join("folder_a");
        let dir_b = base_dir.join("folder_b");
        let _ = std::fs::create_dir_all(&dir_a);
        let _ = std::fs::create_dir_all(&dir_b);

        // Directory A files: PDF, TXT
        let pdf_a = dir_a.join("report_a.pdf");
        let pdf_content = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (etsittava_termi pdf A) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_a, pdf_content).unwrap();

        let txt_a = dir_a.join("notes_a.txt");
        std::fs::write(&txt_a, "etsittava_termi teksti A").unwrap();

        // Directory B files: PDF, MD (text), Flat ODF XML
        let pdf_b = dir_b.join("doc_b.pdf");
        let pdf_content_b = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (etsittava_termi pdf B) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_b, pdf_content_b).unwrap();
        eprintln!("Direct extract_pdf on pdf_b: {:?}", extract_pdf(&pdf_b));

        let md_b = dir_b.join("readme_b.md");
        std::fs::write(&md_b, "etsittava_termi markdown B").unwrap();

        let fodt_b = dir_b.join("letter_b.fodt");
        let fodt_content = r#"<?xml version="1.0" encoding="UTF-8"?>
        <office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                         xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
            <office:body><office:text><text:p>etsittava_termi fodt B</text:p></office:text></office:body>
        </office:document>"#;
        std::fs::write(&fodt_b, fodt_content).unwrap();

        let cache = DocumentCache::new();

        // 1. First search in Directory A
        let mut opts = SearchOptions {
            directory: dir_a.clone(),
            query: "etsittava_termi".to_string(),
            search_docx: true,
            search_odt: true,
            search_pdf: true,
            search_txt: true,
            ..Default::default()
        };

        let matches_a = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_a = matches_a.clone();
        let stats_a = search_directory(&opts, &cache, None, move |r| m_a.lock().unwrap().push(r), |_| {}, |_, _| {}).unwrap();

        let results_a = matches_a.lock().unwrap();
        assert_eq!(results_a.len(), 2, "Dir A should find both PDF and TXT");
        assert_eq!(stats_a.total_count, 2);

        // 2. Change path to Directory B (simulating path switch in UI)
        opts.directory = dir_b.clone();

        let matches_b = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_b = matches_b.clone();
        let stats_b = search_directory(
            &opts,
            &cache,
            None,
            move |r| m_b.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        ).unwrap();

        let results_b = matches_b.lock().unwrap();
        assert_eq!(results_b.len(), 3, "Dir B should find PDF, MD, and FODT (not just txt)");
        let types_b: Vec<String> = results_b.iter().map(|r| r.file_type.clone()).collect();
        assert!(types_b.contains(&"PDF".to_string()));
        assert!(types_b.contains(&"MD".to_string()));
        assert!(types_b.contains(&"FODT".to_string()));
        assert_eq!(stats_b.total_count, 3);

        let _ = std::fs::remove_dir_all(base_dir);
    }
}
