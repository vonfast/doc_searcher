// src/search.rs - Search logic module

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rayon::prelude::*;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::ZipArchive;

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub file: PathBuf,
    pub file_type: String,
    pub matches: Vec<Match>,
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
    pub context_size: usize,
    pub search_docx: bool,
    pub search_odt: bool,
    pub search_pdf: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("."),
            query: String::new(),
            ignore_case: false,
            recursive: true,
            context_size: 150,
            search_docx: true,
            search_odt: true,
            search_pdf: true,
        }
    }
}

pub fn search_directory(
    opts: &SearchOptions,
    progress_cb: impl Fn(String) + Sync + Send,
) -> Result<(Vec<SearchResult>, Vec<SearchError>)> {
    let max_depth = if opts.recursive { usize::MAX } else { 1 };

    let entries: Vec<_> = WalkDir::new(&opts.directory)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();

    let (results_res, errors_res): (Vec<_>, Vec<_>) = entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            let should_search = match ext.as_str() {
                "docx" => opts.search_docx,
                "odt" => opts.search_odt,
                "pdf" => opts.search_pdf,
                _ => false,
            };

            if !should_search {
                return None;
            }

            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            progress_cb(format!("Reading: {}", filename));

            let text_result = match ext.as_str() {
                "docx" => extract_docx(path),
                "odt" => extract_odt(path),
                "pdf" => extract_pdf(path),
                _ => return None,
            };

            match text_result {
                Ok(text) => {
                    let matches = find_matches(&text, &opts.query, opts.ignore_case, opts.context_size);
                    if !matches.is_empty() {
                        Some(Ok(SearchResult {
                            file: path.to_path_buf(),
                            file_type: ext.to_uppercase(),
                            matches,
                        }))
                    } else {
                        None
                    }
                }
                Err(e) => Some(Err(SearchError {
                    file: path.to_path_buf(),
                    error: e.to_string(),
                })),
            }
        })
        .partition(Result::is_ok);

    let results = results_res.into_iter().map(Result::unwrap).collect();
    let errors = errors_res.into_iter().map(Result::unwrap_err).collect();

    Ok((results, errors))
}

/// Extract text from a .docx file
pub fn extract_docx(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let mut xml_content = String::new();
    {
        let mut doc = archive
            .by_name("word/document.xml")
            .with_context(|| "word/document.xml missing from docx")?;
        doc.read_to_string(&mut xml_content)?;
    }
    extract_text_from_xml(&xml_content)
}

/// Extract text from a .odt file
pub fn extract_odt(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(file)
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let mut xml_content = String::new();
    {
        let mut doc = archive
            .by_name("content.xml")
            .with_context(|| "content.xml missing from odt")?;
        doc.read_to_string(&mut xml_content)?;
    }
    extract_text_from_xml(&xml_content)
}

fn extract_text_from_xml(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(true);

    let mut text_content = String::with_capacity(xml.len() / 2);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default();
                if !text.trim().is_empty() {
                    text_content.push_str(&text);
                }
            }
            Ok(Event::End(_)) => {
                text_content.push(' ');
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(text_content)
}

/// Extract text from a .pdf file
pub fn extract_pdf(path: &Path) -> Result<String> {
    pdf_extract::extract_text(path)
        .with_context(|| format!("PDF text extraction failed: {}", path.display()))
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn starts_with_ignore_case(text_slice: &str, query_lower: &str) -> Option<usize> {
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

pub fn find_matches(
    text: &str,
    query: &str,
    ignore_case: bool,
    context_size: usize,
) -> Vec<Match> {
    let mut matches = Vec::new();

    if query.is_empty() {
        return matches;
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
        // 'ẞ' (LATIN CAPITAL LETTER SHARP S) lowercase is 'große' in Rust
        let text = "GROẞE";
        let query = "große";
        let matches = find_matches(text, query, true, 10);
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn test_overlap_avoidance() {
        let text = "aaaaa";
        let query = "aa";
        // Jos hyppäämme koko sanan yli, "aaaaa" hakusanalla "aa" pitäisi löytää 2 osumaa:
        // 1. "aa" kohdassa 0
        // 2. "aa" kohdassa 2 (kohta 1 ohitetaan koska se on osa ensimmäistä osumaa)
        let matches = find_matches(text, query, false, 10);
        assert_eq!(matches.len(), 2);
    }

    #[test]
    fn test_boundary_panic_prevention() {
        // Testataan että ei kaadu jos hakusana on merkin keskellä
        let text = "🦀🦀🦀";
        let query = "🦀";
        let matches = find_matches(text, query, false, 10);
        assert_eq!(matches.len(), 3);
    }
}
