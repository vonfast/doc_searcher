// src/search.rs - Search logic module

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
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
    progress_cb: impl Fn(String),
) -> Result<(Vec<SearchResult>, Vec<SearchError>)> {
    let mut results = Vec::new();
    let mut errors = Vec::new();
    let max_depth = if opts.recursive { usize::MAX } else { 1 };

    let entries: Vec<_> = WalkDir::new(&opts.directory)
        .max_depth(max_depth)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .collect();

    for entry in &entries {
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
            continue;
        }

        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        progress_cb(format!("Reading: {}", filename));

        let text_result = match ext.as_str() {
            "docx" => Some(extract_docx(path)),
            "odt" => Some(extract_odt(path)),
            "pdf" => Some(extract_pdf(path)),
            _ => None,
        };

        match text_result {
            Some(Ok(text)) => {
                let matches = find_matches(&text, &opts.query, opts.ignore_case, opts.context_size);
                if !matches.is_empty() {
                    results.push(SearchResult {
                        file: path.to_path_buf(),
                        file_type: ext.to_uppercase(),
                        matches,
                    });
                }
            }
            Some(Err(e)) => {
                errors.push(SearchError {
                    file: path.to_path_buf(),
                    error: e.to_string(),
                });
            }
            None => {}
        }
    }

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

    let mut text_parts = Vec::new();
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Text(e)) => {
                let text = e.unescape().unwrap_or_default();
                if !text.trim().is_empty() {
                    text_parts.push(text.into_owned());
                }
            }
            Ok(Event::End(_)) => {
                text_parts.push(" ".to_string());
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(text_parts.join(""))
}

/// Extract text from a .pdf file
pub fn extract_pdf(path: &Path) -> Result<String> {
    pdf_extract::extract_text(path)
        .with_context(|| format!("PDF text extraction failed: {}", path.display()))
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

    let q = if ignore_case {
        query.to_lowercase()
    } else {
        query.to_string()
    };

    let mut it = text.char_indices().peekable();

    while let Some(&(start_idx, _)) = it.peek() {
        let matched = if ignore_case {
            text[start_idx..]
                .to_lowercase()
                .starts_with(&q)
        } else {
            text[start_idx..].starts_with(&q)
        };

        if matched {
            let mut end_idx = start_idx;
            let mut match_len_in_lowercase = 0;
            let mut temp_it = text[start_idx..].chars();
            
            // We need to find how many characters from the original text 
            // correspond to the match.
            // This is tricky because one character could become multiple in lowercase.
            while match_len_in_lowercase < q.len() {
                if let Some(c) = temp_it.next() {
                    let lowered = if ignore_case { c.to_lowercase().to_string() } else { c.to_string() };
                    match_len_in_lowercase += lowered.len();
                    end_idx += c.len_utf8();
                } else {
                    break;
                }
            }

            let ctx_start_idx = start_idx.saturating_sub(context_size);
            let ctx_end_idx = (end_idx + context_size).min(text.len());

            // Align to character boundaries
            let actual_start = text.char_indices()
                .map(|(i, _)| i)
                .rfind(|&i| i <= ctx_start_idx)
                .unwrap_or(0);
            
            let actual_end = text.char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(text.len()))
                .find(|&i| i >= ctx_end_idx)
                .unwrap_or(text.len());

            let mut context = String::new();
            if actual_start > 0 {
                context.push_str("… ");
            }
            context.push_str(&text[actual_start..actual_end]);
            if actual_end < text.len() {
                context.push_str(" …");
            }

            let context = context.split_whitespace().collect::<Vec<_>>().join(" ");
            matches.push(Match { context });

            // Move past the match
            while let Some(&(i, _)) = it.peek() {
                if i < end_idx {
                    it.next();
                } else {
                    break;
                }
            }
        } else {
            it.next();
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
