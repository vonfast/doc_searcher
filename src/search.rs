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
    progress_cb: impl Fn(usize, usize) + Sync + Send,
) -> Result<(Vec<SearchResult>, Vec<SearchError>)> {
    let max_depth = if opts.recursive { usize::MAX } else { 1 };

    let entries: Vec<_> = WalkDir::new(&opts.directory)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| {
            if !e.file_type().is_file() {
                return false;
            }
            let ext = e
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_lowercase();
            match ext.as_str() {
                "docx" => opts.search_docx,
                "odt" => opts.search_odt,
                "pdf" => opts.search_pdf,
                _ => false,
            }
        })
        .collect();

    let total = entries.len();
    if total == 0 {
        progress_cb(0, 0);
        return Ok((Vec::new(), Vec::new()));
    }

    let processed = std::sync::atomic::AtomicUsize::new(0);

    let (results_res, errors_res): (Vec<_>, Vec<_>) = entries
        .into_par_iter()
        .filter_map(|entry| {
            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            let text_result = match ext.as_str() {
                "docx" => extract_docx(path),
                "odt" => extract_odt(path),
                "pdf" => extract_pdf(path),
                _ => return None,
            };

            let current = processed.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if current.is_multiple_of(10) || current == total {
                progress_cb(current, total);
            }

            let modified = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());

            match text_result {
                Ok(text) => {
                    let matches = find_matches(&text, &opts.query, opts.ignore_case, opts.context_size);
                    if !matches.is_empty() {
                        Some(Ok(SearchResult {
                            file: path.to_path_buf(),
                            file_type: ext.to_uppercase(),
                            matches,
                            modified,
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

    let mut results: Vec<SearchResult> = results_res.into_iter().map(Result::unwrap).collect();
    results.sort_by(|a, b| b.modified.cmp(&a.modified));

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

pub fn extract_text_from_xml(xml: &str) -> Result<String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(false);

    let mut text_content = String::with_capacity(xml.len() / 2);
    let mut buf = Vec::new();

    let mut in_text_node = false;
    let mut in_paragraph = false;
    let is_docx = xml.contains("w:document") || xml.contains("w:p") || xml.contains("w:t");

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
}
