// src/search.rs - Search logic module

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use walkdir::WalkDir;
use zip::ZipArchive;

struct Semaphore {
    max: usize,
    count: Mutex<usize>,
    cvar: Condvar,
}

impl Semaphore {
    fn new(max: usize) -> Self {
        Self {
            max,
            count: Mutex::new(0),
            cvar: Condvar::new(),
        }
    }

    fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count += 1;
        SemaphoreGuard { sem: self }
    }
}

struct SemaphoreGuard<'a> {
    sem: &'a Semaphore,
}

impl<'a> Drop for SemaphoreGuard<'a> {
    fn drop(&mut self) {
        let mut count = self.sem.count.lock().unwrap_or_else(|e| e.into_inner());
        *count = count.saturating_sub(1);
        self.sem.cvar.notify_one();
    }
}

static PDF_SEMAPHORE: OnceLock<Semaphore> = OnceLock::new();

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct CacheKey {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

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
        // Only cache documents up to 5 MB of text to prevent memory ballooning
        if text.len() > 5 * 1024 * 1024 {
            return;
        }
        if let Ok(mut guard) = self.entries.write() {
            if guard.len() >= 2000 {
                // Remove roughly half of the entries instead of wiping out everything
                let to_remove: Vec<CacheKey> = guard.keys().take(1000).cloned().collect();
                for k in to_remove {
                    guard.remove(&k);
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
            let is_dir = e.file_type().is_dir();
            let is_file = e.file_type().is_file();
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
                    "docx" => opts.search_docx,
                    "odt" => opts.search_odt,
                    "pdf" => opts.search_pdf,
                    "txt" | "md" | "csv" | "log" | "json" => opts.search_txt,
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
            if !e.file_type().is_file() {
                return None;
            }
            let path = e.path();
            let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let ext = canonical_path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            let meta = e.metadata().ok();
            let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let modified = meta.and_then(|m| m.modified().ok());

            if let Some(max_mb) = opts.max_file_size_mb {
                let max_bytes = max_mb.saturating_mul(1024 * 1024);
                if size > max_bytes {
                    return None;
                }
            }

            if let Some(after) = opts.modified_after {
                if let Some(mtime) = modified {
                    if mtime < after {
                        return None;
                    }
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
                        "docx" => extract_docx(&candidate.path),
                        "odt" => extract_odt(&candidate.path),
                        "pdf" => extract_pdf(&candidate.path),
                        "txt" | "md" | "csv" | "log" | "json" => extract_plain_text(&candidate.path),
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
                    "docx" => extract_docx(&candidate.path).map(|t| t.into()),
                    "odt" => extract_odt(&candidate.path).map(|t| t.into()),
                    "pdf" => extract_pdf(&candidate.path).map(|t| t.into()),
                    "txt" | "md" | "csv" | "log" | "json" => extract_plain_text(&candidate.path).map(|t| t.into()),
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
                        on_match(SearchResult {
                            file: candidate.path,
                            file_type: candidate.ext.to_uppercase(),
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

/// Extract text from plain text files (.txt, .md, .csv, .log, .json)
pub fn extract_plain_text(path: &Path) -> Result<String> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 100 * 1024 * 1024 {
        return Err(anyhow::anyhow!("Tekstitiedosto on liian suuri (>100 MB): {}", path.display()));
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("Could not read file: {}", path.display()))?;

    // 1. Try UTF-8 first
    if let Ok(s) = std::str::from_utf8(&bytes) {
        return Ok(s.to_string());
    }

    // 2. If not UTF-8, decode as ISO-8859-1 (Latin-1) where bytes 0x00..=0xFF map 1:1 to Unicode U+0000..=U+00FF.
    // This preserves Nordic characters (ä, ö, å, Ä, Ö, Å) in legacy text/csv files.
    let latin1: String = bytes.iter().map(|&b| b as char).collect();
    Ok(latin1)
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
            let n = name.to_lowercase();
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
        if a.ends_with("document.xml") {
            std::cmp::Ordering::Less
        } else if b.ends_with("document.xml") {
            std::cmp::Ordering::Greater
        } else {
            a.cmp(b)
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

/// Extract text from a .pdf file with header sanitization, concurrency throttling, panic safety, and fallback stream parsing
pub fn extract_pdf(path: &Path) -> Result<String> {
    if std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) > 100 * 1024 * 1024 {
        return Err(anyhow::anyhow!("PDF-tiedosto on liian suuri (>100 MB): {}", path.display()));
    }

    // Limit concurrent PDF parsing to at most 2 threads to prevent massive RAM spikes (OOM)
    let sem = PDF_SEMAPHORE.get_or_init(|| Semaphore::new(2));
    let _guard = sem.acquire();

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

                let stream_content = decompressed.unwrap_or(stream_bytes);
                extract_text_from_pdf_stream(stream_content, &mut extracted_text);

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

pub fn starts_with_ignore_case(text_slice: &str, query_lower: &str) -> Option<usize> {
    let mut text_chars = text_slice.chars();
    let mut query_chars = query_lower.chars();
    let mut bytes_read = 0;

    while let Some(q_char) = query_chars.next() {
        if let Some(t_char) = text_chars.next() {
            bytes_read += t_char.len_utf8();
            let mut lowered = t_char.to_lowercase();
            let first_lowered = lowered.next()?;
            if first_lowered != q_char {
                return None;
            }
            for extra_lowered in lowered {
                if query_chars.next() != Some(extra_lowered) {
                    return None;
                }
            }
        } else {
            return None;
        }
    }
    Some(bytes_read)
}

pub fn find_match_spans(
    text: &str,
    query: &str,
    ignore_case: bool,
) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    if query.is_empty() {
        return spans;
    }

    let q_lower = query.to_lowercase();
    let mut char_indices = text.char_indices().peekable();

    while let Some(&(start_idx, _)) = char_indices.peek() {
        let match_bytes = if ignore_case {
            starts_with_ignore_case(&text[start_idx..], &q_lower)
        } else if text[start_idx..].starts_with(query) {
            Some(query.len())
        } else {
            None
        };

        if let Some(bytes) = match_bytes {
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
    let spans = find_match_spans(text, query, ignore_case);
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
        for (i, word) in sub_slice.split_whitespace().enumerate() {
            if i > 0 {
                context.push(' ');
            }
            context.push_str(word);
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
        for i in 0..2050 {
            let key = CacheKey {
                path: PathBuf::from(format!("/tmp/doc_{}.docx", i)),
                size: 100,
                modified: None,
            };
            cache.insert(key, format!("Content {}", i).into());
        }
        // Should have pruned some elements, but NOT cleared everything to 0
        assert!(cache.len() > 1000);
        assert!(cache.len() <= 2000);
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
}
