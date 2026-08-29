// src/search.rs - Search logic module

use crate::AppLanguage;
use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rayon::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use zip::ZipArchive;

pub const MAX_PDF_FILE_SIZE: u64 = 100 * 1024 * 1024; // 100 MB max
pub const HEAVY_FILE_SIZE_THRESHOLD: u64 = 4 * 1024 * 1024; // 4 MB threshold for heavy permit
pub const MAX_CONCURRENT_HEAVY_EXTRACTIONS: usize = 2;

/// RAII permit for a heavy document extraction task.
pub struct HeavyTaskPermit<'a> {
    semaphore: &'a HeavyTaskSemaphore,
}

impl<'a> Drop for HeavyTaskPermit<'a> {
    fn drop(&mut self) {
        let mut count = self
            .semaphore
            .permits
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *count += 1;
        self.semaphore.cvar.notify_one();
    }
}

/// Concurrency limiter for heavy extraction tasks (large PDFs, large files) across threadpools.
pub struct HeavyTaskSemaphore {
    permits: Mutex<usize>,
    cvar: Condvar,
    #[allow(dead_code)]
    max_permits: usize,
}

impl HeavyTaskSemaphore {
    pub fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits),
            cvar: Condvar::new(),
            max_permits: permits,
        }
    }

    /// Acquires a permit, waking up periodically to check for search cancellation.
    pub fn acquire_cancellable(
        &self,
        is_cancelled: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<HeavyTaskPermit<'_>> {
        let mut count = self
            .permits
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        loop {
            if let Some(cancel) = is_cancelled {
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return None;
                }
            }
            if *count > 0 {
                *count -= 1;
                return Some(HeavyTaskPermit { semaphore: self });
            }
            let (new_count, _) = self
                .cvar
                .wait_timeout(count, std::time::Duration::from_millis(50))
                .ok()?;
            count = new_count;
        }
    }

    #[allow(dead_code)]
    pub fn available_permits(&self) -> usize {
        *self
            .permits
            .lock()
            .map(|g| g)
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[derive(Hash, PartialEq, Eq, Clone, Debug)]
pub struct CacheKey {
    pub path: PathBuf,
    pub size: u64,
    pub modified: Option<std::time::SystemTime>,
}

const NUM_SHARDS: usize = 16;
pub const MAX_CACHE_TOTAL_BYTES: usize = 256 * 1024 * 1024; // 256 MB
pub const MAX_CACHED_ENTRY_BYTES: usize = 5 * 1024 * 1024; // 5 MB per file
pub const MAX_CACHE_ENTRIES: usize = 10_000;
const MAX_SHARD_BYTES: usize = MAX_CACHE_TOTAL_BYTES / NUM_SHARDS;
const MAX_SHARD_ENTRIES: usize = MAX_CACHE_ENTRIES / NUM_SHARDS;

pub fn clean_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p
    }
}

#[derive(Debug, Clone)]
pub struct SearchCandidate {
    pub path: PathBuf,
    pub ext: String,
}

static GLOBAL_CACHE_TICK: AtomicU64 = AtomicU64::new(1);

struct CacheEntry {
    text: Arc<str>,
    size: usize,
    last_accessed: AtomicU64,
}

#[derive(Default)]
struct CacheShard {
    entries: HashMap<CacheKey, CacheEntry>,
    total_bytes: usize,
}

impl CacheShard {
    #[inline]
    fn entry_size(key: &CacheKey, text: &str) -> usize {
        std::mem::size_of::<CacheKey>() + key.path.as_os_str().len() + text.len()
    }

    fn get(&self, key: &CacheKey) -> Option<Arc<str>> {
        if let Some(entry) = self.entries.get(key) {
            let tick = GLOBAL_CACHE_TICK.fetch_add(1, Ordering::Relaxed);
            entry.last_accessed.store(tick, Ordering::Relaxed);
            Some(Arc::clone(&entry.text))
        } else {
            None
        }
    }

    fn insert(&mut self, key: CacheKey, text: Arc<str>) {
        if text.len() > MAX_CACHED_ENTRY_BYTES {
            return;
        }

        let new_entry_size = Self::entry_size(&key, &text);

        // If key already existed, subtract old size first
        if let Some(old_val) = self.entries.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(old_val.size);
        }

        // If exceeding max entries or total bytes in this shard, prune down to 80% using LRU order
        if self.entries.len() >= MAX_SHARD_ENTRIES
            || self.total_bytes + new_entry_size > MAX_SHARD_BYTES
        {
            let target_bytes = (MAX_SHARD_BYTES * 8) / 10;
            let target_entries = (MAX_SHARD_ENTRIES * 8) / 10;

            let bytes_to_free = (self.total_bytes + new_entry_size).saturating_sub(target_bytes);
            let entries_to_free = (self.entries.len() + 1).saturating_sub(target_entries);

            // Collect candidate references without cloning all CacheKey objects
            let mut candidates: Vec<(&CacheKey, u64, usize)> = self
                .entries
                .iter()
                .map(|(k, v)| (k, v.last_accessed.load(Ordering::Relaxed), v.size))
                .collect();

            // Sort by last_accessed ascending (oldest / least recently used first)
            candidates.sort_unstable_by_key(|c| c.1);

            let mut freed_bytes = 0;
            let mut freed_entries = 0;
            let mut keys_to_remove = Vec::new();

            for (k, _, sz) in candidates {
                if freed_bytes < bytes_to_free || freed_entries < entries_to_free {
                    keys_to_remove.push((*k).clone());
                    freed_bytes = freed_bytes.saturating_add(sz);
                    freed_entries += 1;
                } else {
                    break;
                }
            }

            for k in keys_to_remove {
                if let Some(v) = self.entries.remove(&k) {
                    self.total_bytes = self.total_bytes.saturating_sub(v.size);
                }
            }
        }

        let tick = GLOBAL_CACHE_TICK.fetch_add(1, Ordering::Relaxed);
        self.total_bytes = self.total_bytes.saturating_add(new_entry_size);
        self.entries.insert(
            key,
            CacheEntry {
                text,
                size: new_entry_size,
                last_accessed: AtomicU64::new(tick),
            },
        );
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

pub struct DocumentCache {
    shards: Arc<[RwLock<CacheShard>; NUM_SHARDS]>,
}

impl Clone for DocumentCache {
    fn clone(&self) -> Self {
        Self {
            shards: Arc::clone(&self.shards),
        }
    }
}

impl Default for DocumentCache {
    fn default() -> Self {
        Self::new()
    }
}

impl DocumentCache {
    pub fn new() -> Self {
        let shards: [RwLock<CacheShard>; NUM_SHARDS] = Default::default();
        Self {
            shards: Arc::new(shards),
        }
    }

    #[inline]
    fn shard_index(key: &CacheKey) -> usize {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        (hasher.finish() as usize) % NUM_SHARDS
    }

    pub fn get(&self, key: &CacheKey) -> Option<Arc<str>> {
        let idx = Self::shard_index(key);
        let guard = self.shards[idx].read().ok()?;
        guard.get(key)
    }

    pub fn insert(&self, key: CacheKey, text: Arc<str>) {
        let idx = Self::shard_index(&key);
        if let Ok(mut guard) = self.shards[idx].write() {
            guard.insert(key, text);
        }
    }

    pub fn clear(&self) {
        for shard in self.shards.iter() {
            if let Ok(mut guard) = shard.write() {
                guard.clear();
            }
        }
    }

    pub fn len(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.read().map(|g| g.len()).unwrap_or(0))
            .sum()
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn memory_usage_bytes(&self) -> usize {
        self.shards
            .iter()
            .map(|shard| shard.read().map(|g| g.total_bytes()).unwrap_or(0))
            .sum()
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
    pub lang: AppLanguage,
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
            lang: AppLanguage::English,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SearchStats {
    pub cached_count: usize,
    pub total_count: usize,
    pub duration: std::time::Duration,
}

pub fn scan_candidates(
    canonical_root: &Path,
    opts: &SearchOptions,
    is_cancelled: Option<&std::sync::atomic::AtomicBool>,
) -> Vec<SearchCandidate> {
    let max_depth = if opts.recursive { usize::MAX } else { 1 };
    let search_hidden = opts.search_hidden;
    let search_docx = opts.search_docx;
    let search_odt = opts.search_odt;
    let search_pdf = opts.search_pdf;
    let search_txt = opts.search_txt;
    let max_file_size_mb = opts.max_file_size_mb;

    let walk = jwalk::WalkDirGeneric::<((), ())>::new(canonical_root)
        .max_depth(max_depth)
        .skip_hidden(false)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|child_res| {
                if let Ok(entry) = child_res {
                    let ft = entry.file_type;
                    let is_dir = ft.is_dir() || (ft.is_symlink() && entry.path().is_dir());
                    let name = entry.file_name.to_string_lossy();

                    if !search_hidden && name.starts_with('.') {
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
                    } else {
                        let is_file = ft.is_file() || (ft.is_symlink() && entry.path().is_file());
                        if is_file {
                            let ext = std::path::Path::new(&entry.file_name)
                                .extension()
                                .and_then(|ext| ext.to_str())
                                .unwrap_or("")
                                .to_lowercase();

                            let matches_type = match ext.as_str() {
                                "docx" | "docm" | "dotx" | "dotm" => search_docx,
                                "odt" | "ott" | "ods" | "odp" | "fodt" | "fods" => search_odt,
                                "pdf" => search_pdf,
                                "txt" | "text" => search_txt,
                                _ => false,
                            };

                            if !matches_type {
                                return false;
                            }
                        }
                    }
                    true
                } else {
                    false
                }
            });
        });

    let mut candidates = Vec::new();
    for entry_res in walk {
        if let Some(cancel) = is_cancelled {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }

        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => continue,
        };

        let ft = entry.file_type;
        if !(ft.is_file() || (ft.is_symlink() && entry.path().is_file())) {
            continue;
        }

        let meta_res = entry.metadata().ok();
        let path = clean_path(entry.path());
        let ext = path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_lowercase();

        let matches_type = match ext.as_str() {
            "docx" | "docm" | "dotx" | "dotm" => search_docx,
            "odt" | "ott" | "ods" | "odp" | "fodt" | "fods" => search_odt,
            "pdf" => search_pdf,
            "txt" | "text" => search_txt,
            _ => false,
        };

        if !matches_type {
            continue;
        }

        let meta = meta_res.or_else(|| std::fs::metadata(&path).ok());
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);

        if let Some(max_mb) = max_file_size_mb {
            let max_bytes = max_mb.saturating_mul(1024 * 1024);
            if size > max_bytes {
                continue;
            }
        }

        candidates.push(SearchCandidate { path, ext });
    }

    candidates
}

#[allow(clippy::too_many_arguments)]
fn process_candidate(
    candidate: &SearchCandidate,
    opts: &SearchOptions,
    cache: &DocumentCache,
    heavy_semaphore: &HeavyTaskSemaphore,
    is_cancelled: Option<&std::sync::atomic::AtomicBool>,
    cached_hits: &std::sync::atomic::AtomicUsize,
    on_match: &(impl Fn(SearchResult) + Sync + Send),
    on_error: &(impl Fn(SearchError) + Sync + Send),
) {
    if let Some(cancel) = is_cancelled {
        if cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
    }

    // Verify that the file actually exists on disk before processing or checking cache.
    // If deleted since candidate scanning, ignore it cleanly.
    let meta = match std::fs::metadata(&candidate.path) {
        Ok(m) => m,
        Err(_) => return,
    };
    let current_size = meta.len();
    let current_modified = meta.modified().ok();

    // Date filter: strictly check the live current_modified timestamp from disk
    if let Some(after) = opts.modified_after {
        if !matches!(current_modified, Some(mtime) if mtime >= after) {
            return;
        }
    }

    let cache_key = CacheKey {
        path: candidate.path.clone(),
        size: current_size,
        modified: current_modified,
    };

    let text_result: Result<Arc<str>> = if opts.use_cache {
        if let Some(cached) = cache.get(&cache_key) {
            cached_hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(cached)
        } else {
            let is_heavy = candidate.ext == "pdf" || current_size >= HEAVY_FILE_SIZE_THRESHOLD;
            let _permit = if is_heavy {
                match heavy_semaphore.acquire_cancellable(is_cancelled) {
                    Some(p) => Some(p),
                    None => return, // cancelled while waiting for heavy permit
                }
            } else {
                None
            };

            let raw_res = match candidate.ext.as_str() {
                "fodt" | "fods" => extract_flat_xml(&candidate.path),
                "docx" | "docm" | "dotx" | "dotm" => extract_docx(&candidate.path),
                "odt" | "ott" | "ods" | "odp" => extract_odt(&candidate.path),
                "pdf" => extract_pdf_with_lang(&candidate.path, opts.lang),
                "txt" | "text" => extract_plain_text_with_lang(&candidate.path, opts.lang),
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
        let is_heavy = candidate.ext == "pdf" || current_size >= HEAVY_FILE_SIZE_THRESHOLD;
        let _permit = if is_heavy {
            match heavy_semaphore.acquire_cancellable(is_cancelled) {
                Some(p) => Some(p),
                None => return, // cancelled while waiting for heavy permit
            }
        } else {
            None
        };

        match candidate.ext.as_str() {
            "fodt" | "fods" => extract_flat_xml(&candidate.path).map(|t| t.into()),
            "docx" | "docm" | "dotx" | "dotm" => {
                extract_docx(&candidate.path).map(|t| t.into())
            }
            "odt" | "ott" | "ods" | "odp" => extract_odt(&candidate.path).map(|t| t.into()),
            "pdf" => extract_pdf_with_lang(&candidate.path, opts.lang).map(|t| t.into()),
            "txt" | "text" => extract_plain_text_with_lang(&candidate.path, opts.lang).map(|t| t.into()),
            _ => return,
        }
    };

    match text_result {
        Ok(text) => {
            let matches = find_matches(&text, &opts.query, opts.ignore_case, opts.context_size);
            if !matches.is_empty() {
                let display_type = match candidate.ext.as_str() {
                    "docx" | "docm" | "dotx" | "dotm" => "DOCX".to_string(),
                    "odt" | "ott" | "ods" | "odp" => "ODT".to_string(),
                    "fodt" | "fods" => "FODT".to_string(),
                    "pdf" => "PDF".to_string(),
                    "txt" | "text" => "TXT".to_string(),
                    _ => candidate.ext.to_uppercase(),
                };
                on_match(SearchResult {
                    file: candidate.path.clone(),
                    file_type: display_type,
                    matches,
                    modified: current_modified,
                });
            }
        }
        Err(e) => {
            on_error(SearchError {
                file: candidate.path.clone(),
                error: e.to_string(),
            });
        }
    }
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
    let canonical_root = opts
        .directory
        .canonicalize()
        .unwrap_or_else(|_| opts.directory.clone());

    let entries = scan_candidates(&canonical_root, opts, is_cancelled);

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
    let heavy_semaphore = HeavyTaskSemaphore::new(MAX_CONCURRENT_HEAVY_EXTRACTIONS);

    entries.into_par_iter().for_each(|candidate| {
        process_candidate(
            &candidate,
            opts,
            cache,
            &heavy_semaphore,
            is_cancelled,
            &cached_hits,
            &on_match,
            &on_error,
        );

        let current = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        #[allow(clippy::manual_is_multiple_of)]
        if current % step == 0 || current == total {
            progress_cb(current, total);
        }
    });

    Ok(SearchStats {
        cached_count: cached_hits.load(std::sync::atomic::Ordering::Relaxed),
        total_count: total,
        duration: start_time.elapsed(),
    })
}

const MAX_PLAIN_TEXT_FILE_SIZE: u64 = 25 * 1024 * 1024; // 25 MB max

fn format_localized_error(lang: AppLanguage, finnish: &str, english: &str) -> String {
    match lang {
        AppLanguage::Finnish => finnish.to_string(),
        AppLanguage::English => english.to_string(),
    }
}

/// Extract text from plain text files (.txt, .md, .csv, .log, .json)
pub fn extract_plain_text(path: &Path) -> Result<String> {
    extract_plain_text_with_lang(path, AppLanguage::English)
}

pub fn extract_plain_text_with_lang(path: &Path, lang: AppLanguage) -> Result<String> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Could not get file metadata: {}", path.display()))?;
    let file_size = metadata.len();

    if file_size > MAX_PLAIN_TEXT_FILE_SIZE {
        return Err(anyhow::anyhow!(
            format_localized_error(
                lang,
                &format!("Tekstitiedosto on liian suuri (>25 MB): {}", path.display()),
                &format!("Text file is too large (>25 MB): {}", path.display()),
            )
        ));
    }

    if file_size == 0 {
        return Ok(String::new());
    }

    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut bytes = Vec::with_capacity(file_size as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("Could not read file: {}", path.display()))?;

    // Binary check: inspect first 4096 bytes
    let sample = &bytes[..bytes.len().min(4096)];
    let null_count = sample.iter().filter(|&&b| b == 0).count();
    if null_count > 0 {
        return Err(anyhow::anyhow!(
            format_localized_error(
                lang,
                &format!("Tiedosto sisältää binääridataa (nollatavuja): {}", path.display()),
                &format!("File contains binary data (null bytes): {}", path.display()),
            )
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

/// Unescapes XML text directly into the provided output String buffer without per-node heap allocations.
pub fn unescape_xml_into(raw: &[u8], out: &mut String) {
    if !raw.contains(&b'&') {
        // Fast path: No escape entities present (common for >95% of text nodes)
        if let Ok(s) = std::str::from_utf8(raw) {
            out.push_str(s);
        } else {
            out.push_str(&String::from_utf8_lossy(raw));
        }
        return;
    }

    // Escape entities exist: decode directly into `out` without allocating temporary strings
    let mut cursor = 0;
    while cursor < raw.len() {
        if let Some(pos) = raw[cursor..].iter().position(|&b| b == b'&') {
            let amp_pos = cursor + pos;
            // Push text before '&'
            if amp_pos > cursor {
                if let Ok(s) = std::str::from_utf8(&raw[cursor..amp_pos]) {
                    out.push_str(s);
                } else {
                    out.push_str(&String::from_utf8_lossy(&raw[cursor..amp_pos]));
                }
            }

            // Find ending ';' for entity, up to 16 bytes (e.g. &#x10FFFF;)
            let entity_search_slice = &raw[amp_pos + 1..raw.len().min(amp_pos + 16)];
            if let Some(semi_offset) = entity_search_slice.iter().position(|&b| b == b';') {
                let entity_bytes = &raw[amp_pos + 1..amp_pos + 1 + semi_offset];
                match entity_bytes {
                    b"amp" => out.push('&'),
                    b"lt" => out.push('<'),
                    b"gt" => out.push('>'),
                    b"quot" => out.push('"'),
                    b"apos" => out.push('\''),
                    _ if entity_bytes.starts_with(b"#x") || entity_bytes.starts_with(b"#X") => {
                        if let Ok(hex_str) = std::str::from_utf8(&entity_bytes[2..]) {
                            if let Ok(code) = u32::from_str_radix(hex_str, 16) {
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else {
                                out.push('&');
                                if let Ok(s) = std::str::from_utf8(entity_bytes) {
                                    out.push_str(s);
                                }
                                out.push(';');
                            }
                        }
                    }
                    _ if entity_bytes.starts_with(b"#") => {
                        if let Ok(dec_str) = std::str::from_utf8(&entity_bytes[1..]) {
                            if let Ok(code) = dec_str.parse::<u32>() {
                                if let Some(ch) = char::from_u32(code) {
                                    out.push(ch);
                                } else {
                                    out.push('\u{FFFD}');
                                }
                            } else {
                                out.push('&');
                                if let Ok(s) = std::str::from_utf8(entity_bytes) {
                                    out.push_str(s);
                                }
                                out.push(';');
                            }
                        }
                    }
                    _ => {
                        // Unknown named entity: push literal
                        out.push('&');
                        if let Ok(s) = std::str::from_utf8(entity_bytes) {
                            out.push_str(s);
                        }
                        out.push(';');
                    }
                }
                cursor = amp_pos + 1 + semi_offset + 1;
            } else {
                // No terminating ';' found within range: push literal '&' and advance
                out.push('&');
                cursor = amp_pos + 1;
            }
        } else {
            // Remainder of the buffer
            if let Ok(s) = std::str::from_utf8(&raw[cursor..]) {
                out.push_str(s);
            } else {
                out.push_str(&String::from_utf8_lossy(&raw[cursor..]));
            }
            break;
        }
    }
}

/// Extract text from a .docx file using streaming XML reader across all body, header, footer, footnote, endnote, and comment parts
pub fn extract_docx(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    // Collect all XML parts by name: word/document.xml, word/header*.xml, word/footer*.xml, word/footnotes.xml, word/endnotes.xml, word/comments.xml
    let mut xml_entries: Vec<String> = Vec::new();
    for name in archive.file_names() {
        let n = name.replace('\\', "/").to_lowercase();
        if n.starts_with("word/")
            && n.ends_with(".xml")
            && (n == "word/document.xml"
                || n.starts_with("word/header")
                || n.starts_with("word/footer")
                || n.starts_with("word/footnotes")
                || n.starts_with("word/endnotes")
                || n.starts_with("word/comments"))
        {
            xml_entries.push(name.to_string());
        }
    }

    if xml_entries.is_empty() {
        return Err(anyhow::anyhow!(
            "word/document.xml missing from docx: {}",
            path.display()
        ));
    }

    // Ensure word/document.xml comes first
    xml_entries.sort_by(|a, b| {
        let a_norm = a.replace('\\', "/").to_lowercase();
        let b_norm = b.replace('\\', "/").to_lowercase();
        let a_main = a_norm.ends_with("document.xml");
        let b_main = b_norm.ends_with("document.xml");
        match (a_main, b_main) {
            (true, true) => a_norm.cmp(&b_norm),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => a_norm.cmp(&b_norm),
        }
    });

    let mut full_text = String::with_capacity(4096);
    for xml_name in xml_entries {
        let entry = archive.by_name(&xml_name).with_context(|| {
            format!("Could not read part {xml_name} in docx: {}", path.display())
        })?;
        let reader = Reader::from_reader(std::io::BufReader::new(entry));
        let prev_len = full_text.len();
        extract_text_from_xml_reader_into(reader, true, &mut full_text).with_context(|| {
            format!(
                "Error parsing XML in {xml_name} in docx: {}",
                path.display()
            )
        })?;
        if full_text.len() > prev_len && !full_text.ends_with('\n') {
            full_text.push('\n');
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
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let mut full_text = String::with_capacity(4096);

    // 1. content.xml (main document body)
    let doc = archive
        .by_name("content.xml")
        .with_context(|| format!("content.xml missing from odt: {}", path.display()))?;
    let reader = Reader::from_reader(std::io::BufReader::new(doc));
    extract_text_from_xml_reader_into(reader, false, &mut full_text)
        .with_context(|| format!("Error reading content.xml in odt: {}", path.display()))?;

    // 2. styles.xml (contains headers and footers in ODT)
    if let Ok(styles) = archive.by_name("styles.xml") {
        if !full_text.is_empty() && !full_text.ends_with('\n') {
            full_text.push('\n');
        }
        let reader = Reader::from_reader(std::io::BufReader::new(styles));
        let _ = extract_text_from_xml_reader_into(reader, false, &mut full_text);
    }

    Ok(full_text)
}

pub fn extract_text_from_xml_reader<R: std::io::BufRead>(
    reader: Reader<R>,
    is_docx: bool,
) -> Result<String> {
    let mut text_content = String::with_capacity(4096);
    extract_text_from_xml_reader_into(reader, is_docx, &mut text_content)?;
    Ok(text_content)
}

pub fn extract_text_from_xml_reader_into<R: std::io::BufRead>(
    mut reader: Reader<R>,
    is_docx: bool,
    text_content: &mut String,
) -> Result<()> {
    reader.trim_text(false);
    reader.check_end_names(true);

    let mut buf = Vec::with_capacity(512);

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
                    unescape_xml_into(e.as_ref(), text_content);
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
                    } else {
                        text_content.push_str(&String::from_utf8_lossy(e.as_ref()));
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
                                .and_then(|a| {
                                    std::str::from_utf8(&a.value).ok()?.parse::<usize>().ok()
                                })
                                .unwrap_or(1);
                            text_content.reserve(count);
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

    Ok(())
}

#[allow(dead_code)]
pub fn extract_text_from_xml(xml: &str) -> Result<String> {
    let is_docx = xml.contains("w:document") || xml.contains("w:p") || xml.contains("w:t");
    let reader = Reader::from_str(xml);
    extract_text_from_xml_reader(reader, is_docx)
}

const MAX_FALLBACK_STREAM_DECOMPRESSED_BYTES: u64 = 8 * 1024 * 1024; // 8 MB max per stream
const MAX_FALLBACK_TOTAL_EXTRACTED_BYTES: usize = 10 * 1024 * 1024; // 10 MB max total extracted text

/// Extract text from a .pdf file with header sanitization, panic safety, and fallback stream parsing
pub fn extract_pdf(path: &Path) -> Result<String> {
    extract_pdf_with_lang(path, AppLanguage::English)
}

pub fn extract_pdf_with_lang(path: &Path, lang: AppLanguage) -> Result<String> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("Could not get file metadata: {}", path.display()))?;
    let file_size = metadata.len();

    if file_size > MAX_PDF_FILE_SIZE {
        return Err(anyhow::anyhow!(
            format_localized_error(
                lang,
                &format!("PDF-tiedosto on liian suuri (>100 MB): {}", path.display()),
                &format!("PDF file is too large (>100 MB): {}", path.display()),
            )
        ));
    }

    if file_size == 0 {
        return Err(anyhow::anyhow!(
            format_localized_error(
                lang,
                "Tiedosto on tyhjä (0 tavua)",
                "File is empty (0 bytes)",
            )
        ));
    }

    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut bytes = Vec::with_capacity(file_size as usize);
    file.read_to_end(&mut bytes)
        .with_context(|| format!("Could not read file: {}", path.display()))?;

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
        return Err(anyhow::anyhow!(
            "Tiedosto ei ole kelvollinen PDF (puuttuva %PDF-otsake)"
        ));
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
                Err(anyhow::anyhow!(
                    "PDF-tiedoston tekstirakenne on vioittunut tai suojattu"
                ))
            }
        }
    }
}

/// Fallback parser that scans raw PDF streams when xref table or headers are malformed
fn extract_pdf_fallback(bytes: &[u8]) -> Option<String> {
    use std::io::Read;
    let mut extracted_text = String::with_capacity(4096);
    let mut i = 0;
    let len = bytes.len();
    let mut zlib_buf = Vec::new();
    let mut deflate_buf = Vec::new();
    let mut stream_extracted = String::new();

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

            if let Some(end_pos) = bytes[stream_start..]
                .windows(9)
                .position(|w| w == b"endstream")
            {
                let stream_bytes = &bytes[stream_start..stream_start + end_pos];

                // Try decompressing as zlib/flate reusing pre-allocated buffers with bounded memory
                zlib_buf.clear();
                let mut decoder = flate2::read::ZlibDecoder::new(stream_bytes);
                let decompressed = if decoder
                    .by_ref()
                    .take(MAX_FALLBACK_STREAM_DECOMPRESSED_BYTES)
                    .read_to_end(&mut zlib_buf)
                    .is_ok()
                    && !zlib_buf.is_empty()
                {
                    Some(&zlib_buf[..])
                } else {
                    deflate_buf.clear();
                    let mut decoder2 = flate2::read::DeflateDecoder::new(stream_bytes);
                    if decoder2
                        .by_ref()
                        .take(MAX_FALLBACK_STREAM_DECOMPRESSED_BYTES)
                        .read_to_end(&mut deflate_buf)
                        .is_ok()
                        && !deflate_buf.is_empty()
                    {
                        Some(&deflate_buf[..])
                    } else {
                        None
                    }
                };

                stream_extracted.clear();
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

                if extracted_text.len() >= MAX_FALLBACK_TOTAL_EXTRACTED_BYTES {
                    break;
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
    let mut current_str: Vec<u8> = Vec::with_capacity(256);
    let mut hex_str: Vec<u8> = Vec::with_capacity(256);
    let mut decoded: Vec<u8> = Vec::with_capacity(128);

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
                let hex_clean: Vec<u8> = hex_str
                    .iter()
                    .copied()
                    .filter(|c| !c.is_ascii_whitespace())
                    .collect();
                decoded.clear();
                let mut chunk = hex_clean.chunks_exact(2);
                for pair in &mut chunk {
                    if let (Ok(h1), Ok(h2)) = (
                        std::str::from_utf8(&pair[0..1]),
                        std::str::from_utf8(&pair[1..2]),
                    ) {
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

pub fn build_search_regex(query: &str, ignore_case: bool) -> Option<regex::Regex> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parts: Vec<String> = trimmed.split_whitespace().map(regex::escape).collect();
    let pattern = parts.join(r"\s+");
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(ignore_case)
        .build()
        .ok()
}

pub fn find_match_spans(text: &str, query: &str, ignore_case: bool) -> Vec<(usize, usize)> {
    find_match_spans_limit(text, query, ignore_case, None)
}

pub fn find_match_spans_limit(
    text: &str,
    query: &str,
    ignore_case: bool,
    limit: Option<usize>,
) -> Vec<(usize, usize)> {
    let re = match build_search_regex(query, ignore_case) {
        Some(r) => r,
        None => return Vec::new(),
    };

    let mut spans = Vec::new();
    for mat in re.find_iter(text) {
        if let Some(max) = limit {
            if spans.len() >= max {
                break;
            }
        }
        spans.push((mat.start(), mat.end()));
    }

    spans
}

pub fn find_matches(text: &str, query: &str, ignore_case: bool, context_size: usize) -> Vec<Match> {
    find_matches_limit(
        text,
        query,
        ignore_case,
        context_size,
        Some(MAX_MATCHES_PER_FILE),
    )
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
        let extracted =
            extract_text_from_xml_reader(reader, true).expect("Streaming XML extraction failed");
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
        std::fs::write(
            &test_file,
            "Tämä on puhdasta tekstiä testiin.\nToinen rivi.",
        )
        .expect("write failed");

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
            b'S', 0xE4, b'h', b'k', 0xF6, b'p', b'o', b's', b't', b'i', b' ', b'j', b'a', b' ',
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
        assert!(
            result.is_err(),
            "Binary file containing null bytes must be rejected"
        );

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
        let malformed_xml =
            "<w:document><w:body><w:p><w:t>Hello</w:wrong></w:p></w:body></w:document>";
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
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_multi_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // 1. Plain text file (.txt)
        let txt_path = temp_dir.join("sample.txt");
        std::fs::write(&txt_path, "Tämä on hakutermi tekstissä").unwrap();

        // 2. Plain text file (.text)
        let text_path = temp_dir.join("notes.text");
        std::fs::write(&text_path, "Muistiinpanot: hakutermi löytyy täältä").unwrap();

        // 3. Flat ODF XML (.fodt)
        let fodt_path = temp_dir.join("document.fodt");
        let xml_content = r#"<?xml version="1.0" encoding="UTF-8"?>
        <office:document xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
                         xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
            <office:body><office:text><text:p>Tämä on hakutermi ODF-dokumentissa</text:p></office:text></office:body>
        </office:document>"#;
        std::fs::write(&fodt_path, xml_content).unwrap();

        // 4. Mock PDF file with stream text
        let pdf_path = temp_dir.join("doc.pdf");
        let pdf_content = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (hakutermi pdf-tiedostossa) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_path, pdf_content).unwrap();

        // 5. Config/code file (.json, .md) that must be IGNORED
        let json_path = temp_dir.join("config.json");
        std::fs::write(&json_path, "{\"data\": \"hakutermi jsonissa\"}").unwrap();
        let md_path = temp_dir.join("readme.md");
        std::fs::write(&md_path, "# hakutermi markdownissa").unwrap();

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
        )
        .unwrap();

        let results = matches_found.lock().unwrap();
        assert_eq!(
            results.len(),
            4,
            "Should only find 4 supported files (2 TXT, 1 FODT, 1 PDF), ignoring .json and .md"
        );
        let types: Vec<String> = results.iter().map(|r| r.file_type.clone()).collect();
        assert_eq!(types.iter().filter(|t| *t == "TXT").count(), 2);
        assert!(types.contains(&"PDF".to_string()));
        assert!(types.contains(&"FODT".to_string()));
        assert_eq!(stats.total_count, 4);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_repeated_search_with_changing_file_type_options() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_repeat_opts_{}", std::process::id()));
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
        let _ = search_directory(
            &opts1,
            &cache,
            None,
            move |r| m1.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
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
        let _ = search_directory(
            &opts2,
            &cache,
            None,
            move |r| m2.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
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
        let _ = search_directory(
            &opts3,
            &cache,
            None,
            move |r| m3.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(matches3.lock().unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_extract_flat_xml_fodt() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_fodt_test_{}", std::process::id()));
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
        assert_eq!(
            matches2.len(),
            1,
            "Should match multiple spaces in query to multiple spaces in text"
        );

        // Exact case search
        let matches3 = find_matches(text, "tärkeä asiakirja", false, 30);
        assert_eq!(matches3.len(), 1);
    }

    #[test]
    fn test_docx_sort_by_strict_weak_ordering() {
        let mut names = [
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
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_date_test_{}", std::process::id()));
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
        let _ = search_directory(
            &opts,
            &cache,
            None,
            move |r| m_clone.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();

        assert_eq!(matches_found.lock().unwrap().len(), 0);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_directory_change_searches_all_supported_types() {
        let base_dir =
            std::env::temp_dir().join(format!("doxsearch_dir_switch_{}", std::process::id()));
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

        // Directory B files: PDF, TXT, Flat ODF XML
        let pdf_b = dir_b.join("doc_b.pdf");
        let pdf_content_b = b"%PDF-1.4\n1 0 obj\n<< /Length 50 >>\nstream\nBT (etsittava_termi pdf B) Tj ET\nendstream\nendobj\n%%EOF";
        std::fs::write(&pdf_b, pdf_content_b).unwrap();

        let txt_b = dir_b.join("readme_b.txt");
        std::fs::write(&txt_b, "etsittava_termi teksti B").unwrap();

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
        let stats_a = search_directory(
            &opts,
            &cache,
            None,
            move |r| m_a.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();

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
        )
        .unwrap();

        let results_b = matches_b.lock().unwrap();
        assert_eq!(results_b.len(), 3, "Dir B should find PDF, TXT, and FODT");
        let types_b: Vec<String> = results_b.iter().map(|r| r.file_type.clone()).collect();
        assert!(types_b.contains(&"PDF".to_string()));
        assert!(types_b.contains(&"TXT".to_string()));
        assert!(types_b.contains(&"FODT".to_string()));
        assert_eq!(stats_b.total_count, 3);

        let _ = std::fs::remove_dir_all(base_dir);
    }

    #[test]
    fn test_non_txt_files_are_strictly_ignored() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_strict_txt_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // Files that should NOT be matched:
        let extensions = [
            "json", "toml", "yaml", "yml", "xml", "html", "htm", "csv", "tsv", "log", "md",
            "markdown", "ini", "conf", "cfg", "rst", "rs", "py", "sh",
        ];
        for ext in &extensions {
            let p = temp_dir.join(format!("file.{}", ext));
            std::fs::write(&p, "salasana123").unwrap();
        }

        // Only this .txt file should be matched:
        let valid_txt = temp_dir.join("actual_doc.txt");
        std::fs::write(&valid_txt, "salasana123 tekstissä").unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "salasana123".to_string(),
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
            move |r| m_clone.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();

        let results = matches_found.lock().unwrap();
        assert_eq!(
            results.len(),
            1,
            "Only actual_doc.txt should match, all other extensions must be ignored"
        );
        assert_eq!(results[0].file_type, "TXT");
        assert_eq!(stats.total_count, 1);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_sharded_cache_parallel_contention() {
        use std::sync::Arc;
        use std::thread;

        let cache = Arc::new(DocumentCache::new());
        let mut handles = Vec::new();

        for t in 0..16 {
            let cache_clone = Arc::clone(&cache);
            let handle = thread::spawn(move || {
                for i in 0..100 {
                    let key = CacheKey {
                        path: PathBuf::from(format!("/tmp/parallel_doc_{}_{}.txt", t, i)),
                        size: 500,
                        modified: None,
                    };
                    let text: Arc<str> = format!("Tämä on säikeen {} dokumentti {}", t, i).into();
                    cache_clone.insert(key.clone(), text.clone());
                    let retrieved = cache_clone.get(&key);
                    assert!(retrieved.is_some() || !cache_clone.is_empty());
                }
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        assert!(!cache.is_empty());
        assert!(cache.memory_usage_bytes() > 0);
        assert!(cache.memory_usage_bytes() <= MAX_CACHE_TOTAL_BYTES);
    }

    #[test]
    fn test_repeated_search_multi_file_directory_never_wipes_cache() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_multi_cache_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        for i in 0..60 {
            let p = temp_dir.join(format!("file_{}.txt", i));
            std::fs::write(
                &p,
                format!("Dokumentti {} sisältää hakusanan ja uniikkia tekstiä", i),
            )
            .unwrap();
        }

        let cache = DocumentCache::new();
        let opts1 = SearchOptions {
            directory: temp_dir.clone(),
            query: "hakusanan".to_string(),
            use_cache: true,
            ..Default::default()
        };

        // First search: 60 misses, 0 hits
        let stats1 = search_directory(&opts1, &cache, None, |_| {}, |_| {}, |_, _| {}).unwrap();
        assert_eq!(stats1.total_count, 60);
        assert_eq!(stats1.cached_count, 0);
        assert_eq!(cache.len(), 60, "All 60 files must be stored in cache");

        // Second search with different query: 60 hits, 0 misses!
        let opts2 = SearchOptions {
            directory: temp_dir.clone(),
            query: "uniikkia".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats2 = search_directory(&opts2, &cache, None, |_| {}, |_| {}, |_, _| {}).unwrap();
        assert_eq!(stats2.total_count, 60);
        assert_eq!(
            stats2.cached_count, 60,
            "All 60 files must be served directly from cache"
        );
        assert_eq!(cache.len(), 60, "Cache must not be wiped or pruned");

        // Third search with third query: still 100% cache hits!
        let opts3 = SearchOptions {
            directory: temp_dir.clone(),
            query: "Dokumentti".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats3 = search_directory(&opts3, &cache, None, |_| {}, |_| {}, |_, _| {}).unwrap();
        assert_eq!(stats3.total_count, 60);
        assert_eq!(stats3.cached_count, 60);
        assert_eq!(cache.len(), 60);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_jwalk_deep_nested_traversal_and_ignored_dir_pruning() {
        let base_dir = std::env::temp_dir()
            .join(format!("doxsearch_jwalk_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base_dir);

        // 1. Deep nested valid folders
        let deep_dir = base_dir.join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep_dir).unwrap();
        std::fs::write(deep_dir.join("deep_doc.txt"), "Etsittävä termi syvällä").unwrap();

        // 2. Ignored folders that MUST be pruned
        let nm_dir = base_dir.join("web").join("node_modules").join("package");
        std::fs::create_dir_all(&nm_dir).unwrap();
        std::fs::write(nm_dir.join("ignored.txt"), "Etsittävä termi node_modulesissa").unwrap();

        let target_dir = base_dir.join("rust_proj").join("target").join("debug");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("ignored.txt"), "Etsittävä termi targetissa").unwrap();

        let hidden_dir = base_dir.join(".hidden_folder");
        std::fs::create_dir_all(&hidden_dir).unwrap();
        std::fs::write(hidden_dir.join("hidden.txt"), "Etsittävä termi piilossa").unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: base_dir.clone(),
            query: "Etsittävä".to_string(),
            recursive: true,
            search_hidden: false,
            search_txt: true,
            ..Default::default()
        };

        let matches_found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_clone = matches_found.clone();
        let stats = search_directory(
            &opts,
            &cache,
            None,
            move |r| m_clone.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();

        let results = matches_found.lock().unwrap();
        assert_eq!(
            results.len(),
            1,
            "Only deep_doc.txt should be found; node_modules, target, and hidden dirs must be pruned"
        );
        assert_eq!(stats.total_count, 1);
        assert!(results[0].file.ends_with("deep_doc.txt"));

        let _ = std::fs::remove_dir_all(base_dir);
    }

    #[test]
    fn test_heavy_task_semaphore_limiting_and_raii() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use std::thread;

        let semaphore = Arc::new(HeavyTaskSemaphore::new(2));
        let active_tasks = Arc::new(AtomicUsize::new(0));
        let max_observed_concurrent = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();

        for _ in 0..10 {
            let sem = Arc::clone(&semaphore);
            let active = Arc::clone(&active_tasks);
            let max_obs = Arc::clone(&max_observed_concurrent);

            let handle = thread::spawn(move || {
                let permit = sem.acquire_cancellable(None);
                assert!(permit.is_some());

                let cur = active.fetch_add(1, Ordering::SeqCst) + 1;
                max_obs.fetch_max(cur, Ordering::SeqCst);

                // Simulate heavy work
                thread::sleep(std::time::Duration::from_millis(20));

                active.fetch_sub(1, Ordering::SeqCst);
                drop(permit);
            });
            handles.push(handle);
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(semaphore.available_permits(), 2);
        assert!(
            max_observed_concurrent.load(Ordering::SeqCst) <= 2,
            "Concurrent heavy tasks must never exceed semaphore limit (2)"
        );
    }

    #[test]
    fn test_heavy_task_semaphore_cancellation() {
        let semaphore = HeavyTaskSemaphore::new(1);
        let permit = semaphore.acquire_cancellable(None);
        assert!(permit.is_some());

        // Now semaphore is exhausted (0 permits)
        let cancel_flag = std::sync::atomic::AtomicBool::new(true);
        let cancelled_permit = semaphore.acquire_cancellable(Some(&cancel_flag));
        assert!(
            cancelled_permit.is_none(),
            "Cancelled request must abort and return None immediately"
        );
    }

    #[test]
    fn test_heavy_task_semaphore_recovers_from_poisoned_mutex() {
        let semaphore = HeavyTaskSemaphore::new(1);
        let permit = semaphore
            .acquire_cancellable(None)
            .expect("first permit should be acquired");

        let poisoned = std::panic::catch_unwind(|| {
            let _guard = semaphore.permits.lock().unwrap();
            panic!("poison the mutex");
        });
        assert!(poisoned.is_err(), "locking and panicking should poison the mutex");

        drop(permit);
        assert_eq!(
            semaphore.available_permits(),
            1,
            "permit release must recover from poison and restore count"
        );

        let reacquired = semaphore.acquire_cancellable(None);
        assert!(
            reacquired.is_some(),
            "a poisoned mutex should still allow a permit to be reacquired"
        );
    }

    #[test]
    fn test_extract_pdf_oversized_metadata_rejection() {
        let temp_dir = std::env::temp_dir();
        let large_fake_pdf = temp_dir.join("doxsearch_test_huge.pdf");

        // Write a sparse file / truncate file of 105 MB
        let file = std::fs::File::create(&large_fake_pdf).expect("create file failed");
        file.set_len(105 * 1024 * 1024).expect("set_len failed");

        let result = extract_pdf(&large_fake_pdf);
        assert!(
            result.is_err(),
            "Oversized PDF (>100MB) must be rejected immediately via metadata"
        );
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains(">100 MB"));

        let _ = std::fs::remove_file(large_fake_pdf);
    }

    #[test]
    fn test_extract_plain_text_oversized_rejection() {
        let temp_dir = std::env::temp_dir();
        let large_fake_txt = temp_dir.join("doxsearch_test_huge.txt");

        let file = std::fs::File::create(&large_fake_txt).expect("create file failed");
        file.set_len(30 * 1024 * 1024).expect("set_len failed");

        let result = extract_plain_text(&large_fake_txt);
        assert!(
            result.is_err(),
            "Oversized text file (>25MB) must be rejected immediately via metadata"
        );
        let err_msg = result.err().unwrap().to_string();
        assert!(err_msg.contains(">25 MB"));

        let _ = std::fs::remove_file(large_fake_txt);
    }

    #[test]
    fn test_parallel_pdf_search_with_semaphore() {
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_multi_pdf_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // Create 8 small mock PDF files
        for i in 0..8 {
            let pdf_path = temp_dir.join(format!("test_doc_{}.pdf", i));
            let content = format!(
                "%PDF-1.4\n1 0 obj\n<< /Length 60 >>\nstream\nBT (EtsittavaPDFSisalto tiedostossa {}) Tj ET\nendstream\nendobj\n%%EOF",
                i
            );
            std::fs::write(&pdf_path, content.as_bytes()).unwrap();
        }

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "EtsittavaPDFSisalto".to_string(),
            search_docx: false,
            search_odt: false,
            search_pdf: true,
            search_txt: false,
            ..Default::default()
        };

        let matches_found = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let m_clone = matches_found.clone();
        let stats = search_directory(
            &opts,
            &cache,
            None,
            move |r| m_clone.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();

        assert_eq!(stats.total_count, 8);
        assert_eq!(matches_found.lock().unwrap().len(), 8);

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn test_cache_lru_eviction_order() {
        let cache = DocumentCache::new();

        // 1. Create a cold key and a hot key in shard 0
        let mut cold_key = None;
        let mut hot_key = None;

        for i in 0..100_000 {
            let k = CacheKey {
                path: PathBuf::from(format!("/tmp/doc_shard0_{}.txt", i)),
                size: 100,
                modified: None,
            };
            if DocumentCache::shard_index(&k) == 0 {
                if cold_key.is_none() {
                    cold_key = Some(k);
                } else if hot_key.is_none() {
                    hot_key = Some(k);
                    break;
                }
            }
        }

        let cold_key = cold_key.expect("Found cold key");
        let hot_key = hot_key.expect("Found hot key");

        // Insert cold key (will not be accessed again)
        cache.insert(cold_key.clone(), "Cold content".into());

        // Insert hot key
        cache.insert(hot_key.clone(), "Hot content".into());

        // 2. Generate and insert 700+ keys mapping specifically to shard 0 to exceed MAX_SHARD_ENTRIES (625)
        let mut inserted_same_shard = 0;
        let mut i = 1000;
        while inserted_same_shard < MAX_SHARD_ENTRIES + 100 {
            let key = CacheKey {
                path: PathBuf::from(format!("/tmp/doc_filler_{}.txt", i)),
                size: 100,
                modified: None,
            };
            i += 1;
            if DocumentCache::shard_index(&key) == 0 {
                cache.insert(key, format!("Filler {}", i).into());
                inserted_same_shard += 1;
                // Keep hot_key fresh by touching it on every iteration
                let _ = cache.get(&hot_key);
            }
        }

        // 3. Verify LRU eviction results:
        // - Cold key must be evicted because it was never touched after insertion
        assert!(
            cache.get(&cold_key).is_none(),
            "Untouched cold key must be evicted when shard exceeds capacity"
        );
        // - Hot key must still be present because it was kept fresh via recent gets
        assert!(
            cache.get(&hot_key).is_some(),
            "Frequently accessed hot key must be preserved by LRU eviction"
        );
    }

    #[test]
    fn test_unescape_xml_into_entities_and_numeric() {
        let mut out = String::new();

        // 1. Fast path without escape entities
        unescape_xml_into(b"Simple plain text without escapes", &mut out);
        assert_eq!(out, "Simple plain text without escapes");

        // 2. Standard XML entities
        out.clear();
        unescape_xml_into(
            b"Alpha &amp; Beta &lt; Gamma &gt; Delta &quot; Epsilon &apos; Zeta",
            &mut out,
        );
        assert_eq!(out, "Alpha & Beta < Gamma > Delta \" Epsilon ' Zeta");

        // 3. Hexadecimal and decimal numeric character references
        out.clear();
        unescape_xml_into(b"Price: 100 &#x20AC; or &#8364; (Letter: &#x41; / &#65;)", &mut out);
        assert_eq!(out, "Price: 100 € or € (Letter: A / A)");

        // 4. Malformed entities and standalone ampersands
        out.clear();
        unescape_xml_into(b"Rock & Roll &unknown; &amp; End", &mut out);
        assert_eq!(out, "Rock & Roll &unknown; & End");
    }

    #[test]
    fn test_extract_docx_and_odt_archives() {
        use std::io::Write;
        let temp_dir = std::env::temp_dir().join(format!("doxsearch_zip_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        // 1. Create a dummy docx (ZIP archive with word/document.xml and word/header1.xml)
        let docx_path = temp_dir.join("test_sample.docx");
        {
            let file = std::fs::File::create(&docx_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            zip.start_file("word/header1.xml", options).unwrap();
            zip.write_all(b"<w:hdr xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:p><w:r><w:t>Header Text</w:t></w:r></w:p></w:hdr>").unwrap();

            zip.start_file("word/document.xml", options).unwrap();
            zip.write_all(b"<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\"><w:body><w:p><w:r><w:t>Main Document Content</w:t></w:r></w:p></w:body></w:document>").unwrap();

            zip.finish().unwrap();
        }

        let docx_text = extract_docx(&docx_path).expect("Failed to extract docx");
        assert!(docx_text.contains("Main Document Content"));
        assert!(docx_text.contains("Header Text"));

        // 2. Create a dummy odt (ZIP archive with content.xml and styles.xml)
        let odt_path = temp_dir.join("test_sample.odt");
        {
            let file = std::fs::File::create(&odt_path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let options = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);

            zip.start_file("content.xml", options).unwrap();
            zip.write_all(b"<office:document-content xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\"><text:p>ODT Main Content</text:p></office:document-content>").unwrap();

            zip.start_file("styles.xml", options).unwrap();
            zip.write_all(b"<office:document-styles xmlns:text=\"urn:oasis:names:tc:opendocument:xmlns:text:1.0\"><text:p>ODT Header Styles</text:p></office:document-styles>").unwrap();

            zip.finish().unwrap();
        }

        let odt_text = extract_odt(&odt_path).expect("Failed to extract odt");
        assert!(odt_text.contains("ODT Main Content"));
        assert!(odt_text.contains("ODT Header Styles"));

        // Clean up
        let _ = std::fs::remove_file(&docx_path);
        let _ = std::fs::remove_file(&odt_path);
        let _ = std::fs::remove_dir(&temp_dir);
    }

    #[test]
    fn test_new_file_immediately_discovered_on_repeated_search() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_new_file_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let file1 = temp_dir.join("file1.txt");
        std::fs::write(&file1, "Ensimmäinen tiedosto sisältää EtsittavaTeksti").unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "EtsittavaTeksti".to_string(),
            use_cache: true,
            ..Default::default()
        };

        // First search finds file1
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = hits.clone();
        let stats1 = search_directory(
            &opts,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats1.total_count, 1);
        assert_eq!(hits.lock().unwrap().len(), 1);

        // User saves a new file "file2.txt" into the directory
        let file2 = temp_dir.join("file2.txt");
        std::fs::write(&file2, "Toinen tiedosto sisältää myös EtsittavaTeksti").unwrap();

        // Second search must discover the new file IMMEDIATELY without blind spots or snapshot delays
        hits.lock().unwrap().clear();
        let h = hits.clone();
        let stats2 = search_directory(
            &opts,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats2.total_count, 2, "Both files must be found immediately");
        assert_eq!(hits.lock().unwrap().len(), 2, "Newly added file must produce match immediately");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_date_filter_strictly_uses_live_modified() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_live_date_unique_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let file1 = temp_dir.join("file1.txt");
        std::fs::write(&file1, "Sisältö Hakusana").unwrap();

        let cache = DocumentCache::new();
        let future_time = std::time::SystemTime::now() + std::time::Duration::from_secs(3600);
        let past_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);

        let opts_future = SearchOptions {
            directory: temp_dir.clone(),
            query: "Hakusana".to_string(),
            modified_after: Some(future_time),
            ..Default::default()
        };

        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = hits.clone();
        let _ = search_directory(
            &opts_future,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(hits.lock().unwrap().len(), 0, "Future modified_after must exclude current file");

        let opts_past = SearchOptions {
            directory: temp_dir.clone(),
            query: "Hakusana".to_string(),
            modified_after: Some(past_time),
            ..Default::default()
        };
        hits.lock().unwrap().clear();
        let h = hits.clone();
        let _ = search_directory(
            &opts_past,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(hits.lock().unwrap().len(), 1, "Past modified_after must include current file");

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_deleted_and_modified_files_in_repeated_searches() {
        let temp_dir =
            std::env::temp_dir().join(format!("doxsearch_del_mod_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);

        let file1 = temp_dir.join("file1.txt");
        let file2 = temp_dir.join("file2.txt");

        std::fs::write(&file1, "TiedostoYksi sisältää AvainsanaAlkuperainen").unwrap();
        std::fs::write(&file2, "TiedostoKaksi sisältää AvainsanaKaksi").unwrap();

        let cache = DocumentCache::new();
        let opts = SearchOptions {
            directory: temp_dir.clone(),
            query: "Avainsana".to_string(),
            use_cache: true,
            ..Default::default()
        };

        // 1. Initial search caches contents
        let hits = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let h = hits.clone();
        let stats1 = search_directory(
            &opts,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats1.total_count, 2);
        assert_eq!(hits.lock().unwrap().len(), 2);

        // 2. Delete file2
        let _ = std::fs::remove_file(&file2);

        // 3. Modify file1 with new content
        std::thread::sleep(std::time::Duration::from_millis(10)); // Ensure modified time tick differs
        std::fs::write(
            &file1,
            "TiedostoYksi on nyt päivitetty: AvainsanaPaivitetty",
        )
        .unwrap();

        // 4. Search for original keyword "AvainsanaAlkuperainen" -> must NOT match because file1 was modified
        hits.lock().unwrap().clear();
        let h = hits.clone();
        let opts_old = SearchOptions {
            directory: temp_dir.clone(),
            query: "AvainsanaAlkuperainen".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats_old = search_directory(
            &opts_old,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats_old.total_count, 1, "Deleted file must not be counted in total_count");
        assert_eq!(
            hits.lock().unwrap().len(),
            0,
            "Modified file must not hit stale cached content"
        );

        // 5. Search for "AvainsanaKaksi" -> must NOT match because file2 was deleted
        hits.lock().unwrap().clear();
        let h = hits.clone();
        let opts_del = SearchOptions {
            directory: temp_dir.clone(),
            query: "AvainsanaKaksi".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats_del = search_directory(
            &opts_del,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats_del.total_count, 1, "Deleted file must not be counted in total_count");
        assert_eq!(
            hits.lock().unwrap().len(),
            0,
            "Deleted file must not produce match"
        );

        // 6. Search for updated keyword "AvainsanaPaivitetty" -> must match file1
        hits.lock().unwrap().clear();
        let h = hits.clone();
        let opts_new = SearchOptions {
            directory: temp_dir.clone(),
            query: "AvainsanaPaivitetty".to_string(),
            use_cache: true,
            ..Default::default()
        };
        let stats_new = search_directory(
            &opts_new,
            &cache,
            None,
            move |r| h.lock().unwrap().push(r),
            |_| {},
            |_, _| {},
        )
        .unwrap();
        assert_eq!(stats_new.total_count, 1, "Deleted file must not be counted in total_count");
        assert_eq!(
            hits.lock().unwrap().len(),
            1,
            "Modified file must be parsed and found"
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}


