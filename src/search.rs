// src/search.rs - Search logic module

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::ZipArchive;

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
            search_hidden: false,
            context_size: 150,
            search_docx: true,
            search_odt: true,
            search_pdf: true,
        }
    }
}

#[derive(Debug)]
struct SearchCandidate {
    path: PathBuf,
    ext: String,
    modified: Option<std::time::SystemTime>,
}

pub fn search_directory(
    opts: &SearchOptions,
    on_match: impl Fn(SearchResult) + Sync + Send,
    on_error: impl Fn(SearchError) + Sync + Send,
    progress_cb: impl Fn(usize, usize) + Sync + Send,
) -> Result<()> {
    let max_depth = if opts.recursive { usize::MAX } else { 1 };

    let entries: Vec<SearchCandidate> = WalkDir::new(&opts.directory)
        .max_depth(max_depth)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            if e.file_type().is_dir() {
                let name = e.file_name().to_string_lossy();
                if !opts.search_hidden && name.starts_with('.') {
                    return false;
                }
                if name == "node_modules" || name == "target" || name == "__pycache__" {
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
            let ext = path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();

            let matches_type = match ext.as_str() {
                "docx" => opts.search_docx,
                "odt" => opts.search_odt,
                "pdf" => opts.search_pdf,
                _ => false,
            };

            if !matches_type {
                return None;
            }

            let modified = e.metadata().ok().and_then(|m| m.modified().ok());

            Some(SearchCandidate {
                path: path.to_path_buf(),
                ext,
                modified,
            })
        })
        .collect();

    let total = entries.len();
    if total == 0 {
        progress_cb(0, 0);
        return Ok(());
    }

    let processed = std::sync::atomic::AtomicUsize::new(0);

    entries
        .into_par_iter()
        .for_each(|candidate| {
            let text_result = match candidate.ext.as_str() {
                "docx" => extract_docx(&candidate.path),
                "odt" => extract_odt(&candidate.path),
                "pdf" => extract_pdf(&candidate.path),
                _ => return,
            };

            let current = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if current.is_multiple_of(10) || current == total {
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

    Ok(())
}

/// Extract text from a .docx file using streaming XML reader
pub fn extract_docx(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let doc = archive
        .by_name("word/document.xml")
        .with_context(|| "word/document.xml missing from docx")?;
    let reader = Reader::from_reader(std::io::BufReader::new(doc));
    extract_text_from_xml_reader(reader, true)
}

/// Extract text from a .odt file using streaming XML reader
pub fn extract_odt(path: &Path) -> Result<String> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("Could not open file: {}", path.display()))?;
    let mut archive = ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("Could not read ZIP archive: {}", path.display()))?;

    let doc = archive
        .by_name("content.xml")
        .with_context(|| "content.xml missing from odt")?;
    let reader = Reader::from_reader(std::io::BufReader::new(doc));
    extract_text_from_xml_reader(reader, false)
}

pub fn extract_text_from_xml_reader<R: std::io::BufRead>(
    mut reader: Reader<R>,
    is_docx: bool,
) -> Result<String> {
    reader.trim_text(false);

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
            Err(_) => break,
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

/// Extract text from a .pdf file
pub fn extract_pdf(path: &Path) -> Result<String> {
    pdf_extract::extract_text(path)
        .with_context(|| format!("PDF text extraction failed: {}", path.display()))
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
        assert!(opts.search_docx);
        assert!(opts.search_odt);
        assert!(opts.search_pdf);
    }
}
