mod search;

slint::include_modules!();

use chrono::{DateTime, Local};
use search::{DocumentCache, SearchError, SearchOptions, SearchResult, SearchStats};
use slint::{Color, ComponentHandle, SharedString, VecModel};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum AppLanguage {
    Finnish,
    English,
}

impl AppLanguage {
    pub fn detect_from_system() -> Self {
        let lang = std::env::var("LC_ALL")
            .or_else(|_| std::env::var("LC_MESSAGES"))
            .or_else(|_| std::env::var("LANG"))
            .unwrap_or_default()
            .to_lowercase();

        if lang.starts_with("fi") {
            AppLanguage::Finnish
        } else {
            AppLanguage::English
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AppLanguage::Finnish => "Suomi",
            AppLanguage::English => "English",
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum SortOrder {
    DateDesc,
    DateAsc,
    Name,
    Matches,
}

impl SortOrder {
    fn from_index(index: i32) -> Self {
        match index {
            1 => SortOrder::DateAsc,
            2 => SortOrder::Name,
            3 => SortOrder::Matches,
            _ => SortOrder::DateDesc,
        }
    }

    fn options(lang: AppLanguage) -> Vec<SharedString> {
        match lang {
            AppLanguage::Finnish => vec![
                "Päivämäärä (Uusin ensin)".into(),
                "Päivämäärä (Vanhin ensin)".into(),
                "Nimi (A-Z)".into(),
                "Osumien määrä".into(),
            ],
            AppLanguage::English => vec![
                "Date (Newest first)".into(),
                "Date (Oldest first)".into(),
                "Name (A-Z)".into(),
                "Match count".into(),
            ],
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum DateFilter {
    All,
    Last24Hours,
    Last7Days,
    Last30Days,
    LastYear,
}

impl DateFilter {
    fn from_index(index: i32) -> Self {
        match index {
            1 => DateFilter::Last24Hours,
            2 => DateFilter::Last7Days,
            3 => DateFilter::Last30Days,
            4 => DateFilter::LastYear,
            _ => DateFilter::All,
        }
    }

    fn options(lang: AppLanguage) -> Vec<SharedString> {
        match lang {
            AppLanguage::Finnish => vec![
                "Kaikki ajat".into(),
                "Viimeiset 24 tuntia".into(),
                "Viimeiset 7 päivää".into(),
                "Viimeiset 30 päivää".into(),
                "Viimeinen vuosi".into(),
            ],
            AppLanguage::English => vec![
                "All time".into(),
                "Last 24 hours".into(),
                "Last 7 days".into(),
                "Last 30 days".into(),
                "Last year".into(),
            ],
        }
    }

    fn to_system_time(self) -> Option<std::time::SystemTime> {
        let now = std::time::SystemTime::now();
        match self {
            DateFilter::All => None,
            DateFilter::Last24Hours => now.checked_sub(std::time::Duration::from_secs(24 * 3600)),
            DateFilter::Last7Days => now.checked_sub(std::time::Duration::from_secs(7 * 24 * 3600)),
            DateFilter::Last30Days => now.checked_sub(std::time::Duration::from_secs(30 * 24 * 3600)),
            DateFilter::LastYear => now.checked_sub(std::time::Duration::from_secs(365 * 24 * 3600)),
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum SizeFilter {
    NoLimit,
    Max10MB,
    Max50MB,
    Max100MB,
}

impl SizeFilter {
    fn from_index(index: i32) -> Self {
        match index {
            1 => SizeFilter::Max10MB,
            2 => SizeFilter::Max50MB,
            3 => SizeFilter::Max100MB,
            _ => SizeFilter::NoLimit,
        }
    }

    fn options(lang: AppLanguage) -> Vec<SharedString> {
        match lang {
            AppLanguage::Finnish => vec![
                "Ei kokorajoitusta".into(),
                "Enintään 10 MB".into(),
                "Enintään 50 MB".into(),
                "Enintään 100 MB".into(),
            ],
            AppLanguage::English => vec![
                "No size limit".into(),
                "Max 10 MB".into(),
                "Max 50 MB".into(),
                "Max 100 MB".into(),
            ],
        }
    }

    fn to_mb(self) -> Option<u64> {
        match self {
            SizeFilter::NoLimit => None,
            SizeFilter::Max10MB => Some(10),
            SizeFilter::Max50MB => Some(50),
            SizeFilter::Max100MB => Some(100),
        }
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum LinuxFileManager {
    Dolphin,
    Nautilus,
    Nemo,
    Thunar,
    Generic,
}

static LINUX_FM_CACHE: OnceLock<LinuxFileManager> = OnceLock::new();

pub fn detect_linux_file_manager() -> LinuxFileManager {
    *LINUX_FM_CACHE.get_or_init(|| {
        if let Ok(out) = std::process::Command::new("xdg-mime")
            .args(["query", "default", "inode/directory"])
            .output()
        {
            let mime = String::from_utf8_lossy(&out.stdout).to_lowercase();
            if mime.contains("dolphin") {
                return LinuxFileManager::Dolphin;
            } else if mime.contains("nautilus") || mime.contains("gnome") {
                return LinuxFileManager::Nautilus;
            } else if mime.contains("nemo") {
                return LinuxFileManager::Nemo;
            } else if mime.contains("thunar") {
                return LinuxFileManager::Thunar;
            }
        }

        let desktop = std::env::var("XDG_CURRENT_DESKTOP")
            .or_else(|_| std::env::var("DESKTOP_SESSION"))
            .unwrap_or_default()
            .to_uppercase();

        if desktop.contains("KDE") || desktop.contains("PLASMA") {
            LinuxFileManager::Dolphin
        } else if desktop.contains("GNOME") {
            LinuxFileManager::Nautilus
        } else if desktop.contains("CINNAMON") {
            LinuxFileManager::Nemo
        } else if desktop.contains("XFCE") {
            LinuxFileManager::Thunar
        } else {
            LinuxFileManager::Generic
        }
    })
}

pub fn os_file_manager_name(lang: AppLanguage) -> &'static str {
    #[cfg(target_os = "macos")]
    {
        let _ = lang;
        "Finder"
    }

    #[cfg(target_os = "windows")]
    {
        match lang {
            AppLanguage::Finnish => "Resurssienhallinta",
            AppLanguage::English => "File Explorer",
        }
    }

    #[cfg(target_os = "linux")]
    {
        match detect_linux_file_manager() {
            LinuxFileManager::Dolphin => "Dolphin",
            LinuxFileManager::Nautilus => match lang {
                AppLanguage::Finnish => "Tiedostot (Nautilus)",
                AppLanguage::English => "Files (Nautilus)",
            },
            LinuxFileManager::Nemo => "Nemo",
            LinuxFileManager::Thunar => "Thunar",
            LinuxFileManager::Generic => match lang {
                AppLanguage::Finnish => "Tiedostonhallinta",
                AppLanguage::English => "File Manager",
            },
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        match lang {
            AppLanguage::Finnish => "Tiedostonhallinta",
            AppLanguage::English => "File Manager",
        }
    }
}

static USER_HOME_CACHE: OnceLock<PathBuf> = OnceLock::new();
static USER_DOCS_CACHE: OnceLock<PathBuf> = OnceLock::new();
static USER_DL_CACHE: OnceLock<PathBuf> = OnceLock::new();

pub fn get_user_home_dir() -> PathBuf {
    USER_HOME_CACHE
        .get_or_init(|| {
            let home = std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home)
        })
        .clone()
}

pub fn get_user_documents_dir() -> PathBuf {
    USER_DOCS_CACHE
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                if let Ok(out) = std::process::Command::new("xdg-user-dir")
                    .arg("DOCUMENTS")
                    .output()
                {
                    if out.status.success() {
                        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !s.is_empty() {
                            let p = PathBuf::from(&s);
                            if p.exists() {
                                return p;
                            }
                        }
                    }
                }
            }

            #[cfg(target_os = "windows")]
            {
                if let Ok(userprofile) = std::env::var("USERPROFILE") {
                    let onedrive_docs = PathBuf::from(&userprofile)
                        .join("OneDrive")
                        .join("Documents");
                    if onedrive_docs.exists() {
                        return onedrive_docs;
                    }
                    let docs = PathBuf::from(&userprofile).join("Documents");
                    if docs.exists() {
                        return docs;
                    }
                }
            }

            let home = get_user_home_dir();
            let docs = home.join("Documents");
            if docs.exists() {
                docs
            } else {
                home
            }
        })
        .clone()
}

pub fn get_user_downloads_dir() -> PathBuf {
    USER_DL_CACHE
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                if let Ok(out) = std::process::Command::new("xdg-user-dir")
                    .arg("DOWNLOAD")
                    .output()
                {
                    if out.status.success() {
                        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                        if !s.is_empty() {
                            let p = PathBuf::from(&s);
                            if p.exists() {
                                return p;
                            }
                        }
                    }
                }
            }

            let home = get_user_home_dir();
            let dl = home.join("Downloads");
            if dl.exists() {
                dl
            } else {
                home
            }
        })
        .clone()
}

pub fn pick_folder_dialog(start_dir: Option<&Path>) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if detect_linux_file_manager() == LinuxFileManager::Dolphin {
            let start = start_dir
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| get_user_documents_dir().to_string_lossy().to_string());
            if let Ok(out) = std::process::Command::new("kdialog")
                .args(["--getexistingdirectory", &start])
                .output()
            {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !s.is_empty() {
                        let p = PathBuf::from(&s);
                        if p.exists() {
                            return Some(p);
                        }
                    }
                }
                return None;
            }
        }
    }

    let mut dialog = rfd::FileDialog::new();
    if let Some(dir) = start_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

pub fn save_file_dialog(default_name: &str, filter_ext: &str) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if detect_linux_file_manager() == LinuxFileManager::Dolphin {
            let start = get_user_documents_dir().join(default_name);
            if let Ok(out) = std::process::Command::new("kdialog")
                .args([
                    "--getsavefilename",
                    &start.to_string_lossy(),
                    &format!("*.{filter_ext}"),
                ])
                .output()
            {
                if out.status.success() {
                    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !s.is_empty() {
                        return Some(PathBuf::from(&s));
                    }
                }
                return None;
            }
        }
    }

    rfd::FileDialog::new()
        .set_file_name(default_name)
        .add_filter("CSV files", &[filter_ext])
        .save_file()
}

pub fn show_in_file_manager(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Err(format!("Path does not exist: {}", path.display()));
    }

    #[cfg(target_os = "windows")]
    {
        let path_str = path.to_string_lossy();
        let status = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path_str))
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Ok(());
            }
        }
        let parent = path.parent().unwrap_or(path);
        open::that(parent).map_err(|e| format!("Failed to open in Explorer: {e}"))
    }

    #[cfg(target_os = "macos")]
    {
        let status = std::process::Command::new("open")
            .arg("-R")
            .arg(path)
            .status();
        if let Ok(s) = status {
            if s.success() {
                return Ok(());
            }
        }
        let parent = path.parent().unwrap_or(path);
        open::that(parent).map_err(|e| format!("Failed to open in Finder: {e}"))
    }

    #[cfg(target_os = "linux")]
    {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let fm = detect_linux_file_manager();

        match fm {
            LinuxFileManager::Dolphin => {
                if std::process::Command::new("dolphin")
                    .arg("--select")
                    .arg(&canonical)
                    .spawn()
                    .is_ok()
                {
                    return Ok(());
                }
            }
            LinuxFileManager::Nautilus => {
                if std::process::Command::new("nautilus")
                    .arg("--select")
                    .arg(&canonical)
                    .spawn()
                    .is_ok()
                {
                    return Ok(());
                }
            }
            LinuxFileManager::Nemo => {
                if std::process::Command::new("nemo")
                    .arg(&canonical)
                    .spawn()
                    .is_ok()
                {
                    return Ok(());
                }
            }
            _ => {}
        }

        let uri = path_to_file_uri(&canonical);
        let dbus_result = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{}", uri),
                "string:",
            ])
            .status();

        if let Ok(status) = dbus_result {
            if status.success() {
                return Ok(());
            }
        }

        let parent = canonical.parent().unwrap_or(&canonical);
        open::that(parent).map_err(|e| format!("Failed to open folder: {e}"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let parent = path.parent().unwrap_or(path);
        open::that(parent).map_err(|e| format!("Failed to open folder: {e}"))
    }
}

pub fn path_to_file_uri(path: &Path) -> String {
    let path_str = path.to_string_lossy();
    let mut uri = String::from("file://");
    for byte in path_str.as_bytes() {
        match *byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                uri.push(*byte as char);
            }
            b => {
                use std::fmt::Write;
                let _ = write!(uri, "%{:02X}", b);
            }
        }
    }
    uri
}

pub fn copy_to_clipboard(text: &str) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(mut child) = std::process::Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }

        if let Ok(mut child) = std::process::Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }

        if let Ok(mut child) = std::process::Command::new("xsel")
            .args(["--clipboard", "--input"])
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(mut child) = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(mut child) = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

pub fn truncate_path(path_str: &str, max_chars: usize) -> String {
    let char_count = path_str.chars().count();
    if char_count <= max_chars {
        path_str.to_string()
    } else {
        let keep = max_chars.saturating_sub(3);
        let skip = char_count.saturating_sub(keep);
        let suffix: String = path_str.chars().skip(skip).collect();
        format!("...{}", suffix)
    }
}

pub fn clean_path(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        PathBuf::from(stripped)
    } else {
        p
    }
}

pub fn resolve_directory_path(input: &str) -> PathBuf {
    let trimmed = input.trim().trim_matches(|c| c == '\'' || c == '"');
    if trimmed.is_empty() {
        return PathBuf::from(".");
    }
    let p = if trimmed == "~" {
        get_user_home_dir()
    } else if let Some(stripped) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        get_user_home_dir().join(stripped)
    } else {
        PathBuf::from(trimmed)
    };

    let resolved = if let Ok(canon) = p.canonicalize() {
        canon
    } else {
        p
    };
    clean_path(resolved)
}

fn type_badge_colors(file_type: &str) -> (Color, Color) {
    match file_type {
        "DOCX" => (Color::from_rgb_u8(219, 234, 254), Color::from_rgb_u8(29, 78, 216)), // Blue
        "ODT" | "FODT" => (Color::from_rgb_u8(220, 252, 231), Color::from_rgb_u8(21, 128, 61)), // Green
        "PDF" => (Color::from_rgb_u8(255, 237, 213), Color::from_rgb_u8(194, 65, 12)),  // Orange
        "TXT" => (Color::from_rgb_u8(243, 232, 255), Color::from_rgb_u8(126, 34, 206)), // Purple
        _ => (Color::from_rgb_u8(241, 245, 249), Color::from_rgb_u8(71, 85, 105)),
    }
}

fn csv_field(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn save_results_csv(
    path: &Path,
    query: &str,
    results: &[SearchResult],
) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    writer.write_all(b"\xEF\xBB\xBF")?;
    writeln!(writer, "query,file,file_type,match_number,context")?;
    for result in results {
        for (index, found_match) in result.matches.iter().enumerate() {
            writeln!(
                writer,
                "{},{},{},{},{}",
                csv_field(query),
                csv_field(&result.file.to_string_lossy()),
                csv_field(&result.file_type),
                index + 1,
                csv_field(&found_match.context)
            )?;
        }
    }
    writer.flush()?;
    Ok(())
}

struct AppState {
    lang: AppLanguage,
    cache: DocumentCache,
    results: Vec<SearchResult>,
    errors: Vec<SearchError>,
    selected_result_idx: Option<usize>,
    recent_directories: Vec<String>,
    current_search_id: usize,
    cancel_flag: Option<Arc<AtomicBool>>,
    sort_order: SortOrder,
    last_stats: Option<SearchStats>,
}

fn sort_results_list(results: &mut [SearchResult], sort_order: SortOrder) {
    match sort_order {
        SortOrder::DateDesc => {
            results.sort_by(|a, b| match (b.modified, a.modified) {
                (Some(tb), Some(ta)) => tb.cmp(&ta).then_with(|| a.file.cmp(&b.file)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.file.cmp(&b.file),
            });
        }
        SortOrder::DateAsc => {
            results.sort_by(|a, b| match (a.modified, b.modified) {
                (Some(ta), Some(tb)) => ta.cmp(&tb).then_with(|| a.file.cmp(&b.file)),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => a.file.cmp(&b.file),
            });
        }
        SortOrder::Name => {
            results.sort_by(|a, b| {
                let name_a = a.file.file_name().map(|n| n.to_string_lossy().to_lowercase());
                let name_b = b.file.file_name().map(|n| n.to_string_lossy().to_lowercase());
                name_a.cmp(&name_b).then_with(|| a.file.cmp(&b.file))
            });
        }
        SortOrder::Matches => {
            results.sort_by(|a, b| b.matches.len().cmp(&a.matches.len()).then_with(|| a.file.cmp(&b.file)));
        }
    }
}

fn update_ui_language(ui: &AppWindow, lang: AppLanguage) {
    match lang {
        AppLanguage::Finnish => {
            ui.set_current_language("Suomi".into());
            ui.set_text_subtitle("| Etsi tekstiä DOCX-, ODT-, PDF- ja tekstitiedostoista".into());
            ui.set_text_dir_label("Hakemisto".into());
            ui.set_text_browse("Selaa...".into());
            ui.set_text_home("~ Koti".into());
            ui.set_text_docs("Asiakirjat".into());
            ui.set_text_downloads("Lataukset".into());
            ui.set_text_query_label("Hakusana".into());
            ui.set_text_query_hint("Kirjoita hakusana...".into());
            ui.set_text_settings_label("Asetukset".into());
            ui.set_text_ignore_case("Älä huomioi kirjainkokoa".into());
            ui.set_text_recursive("Rekursiivinen haku".into());
            ui.set_text_search_hidden("Hae piilokansioista".into());
            ui.set_text_use_cache("Käytä välimuistia".into());
            ui.set_text_clear_cache("Tyhjennä".into());
            ui.set_text_filetypes_label("Tiedostotyypit:".into());
            ui.set_text_all_filetypes("Kaikki".into());
            ui.set_text_date_range("Aikarajaus:".into());
            ui.set_text_max_size("Maksimikoko:".into());
            ui.set_text_context_size("Kontekstin pituus:".into());
            ui.set_text_search("Hae".into());
            ui.set_text_searching("Haetaan...".into());
            ui.set_text_cancel("Peruuta".into());
            ui.set_text_clear("Tyhjennä".into());
            ui.set_text_save_results("Tallenna tulokset...".into());
            ui.set_text_results_header("Tiedostot".into());
            ui.set_text_open("Avaa".into());
            ui.set_text_open_file("Avaa tiedosto".into());
            ui.set_text_folder("Kansio".into());
            ui.set_text_show_in_folder("Näytä kansiossa".into());
            ui.set_text_copy_path("Kopioi polku".into());
            ui.set_text_copy("Kopioi".into());
            ui.set_text_empty_prompt("Kirjoita hakusana ja paina Hae".into());
            ui.set_text_no_matches("Ei osumia hakusanalle".into());
            ui.set_text_select_file("Valitse tiedosto listasta".into());
            ui.set_text_recent_folders("Viimeisimmät kansiot...".into());
        }
        AppLanguage::English => {
            ui.set_current_language("English".into());
            ui.set_text_subtitle("| Search text in DOCX, ODT, PDF and TXT files".into());
            ui.set_text_dir_label("Directory".into());
            ui.set_text_browse("Browse...".into());
            ui.set_text_home("~ Home".into());
            ui.set_text_docs("Documents".into());
            ui.set_text_downloads("Downloads".into());
            ui.set_text_query_label("Search term".into());
            ui.set_text_query_hint("Type search term...".into());
            ui.set_text_settings_label("Settings".into());
            ui.set_text_ignore_case("Ignore case".into());
            ui.set_text_recursive("Recursive search".into());
            ui.set_text_search_hidden("Search hidden folders".into());
            ui.set_text_use_cache("Use memory cache".into());
            ui.set_text_clear_cache("Clear".into());
            ui.set_text_filetypes_label("File types:".into());
            ui.set_text_all_filetypes("All".into());
            ui.set_text_date_range("Date range:".into());
            ui.set_text_max_size("Max size:".into());
            ui.set_text_context_size("Context size:".into());
            ui.set_text_search("Search".into());
            ui.set_text_searching("Searching...".into());
            ui.set_text_cancel("Cancel".into());
            ui.set_text_clear("Clear".into());
            ui.set_text_save_results("Save results...".into());
            ui.set_text_results_header("Files".into());
            ui.set_text_open("Open".into());
            ui.set_text_open_file("Open File".into());
            ui.set_text_folder("Folder".into());
            ui.set_text_show_in_folder("Show in Folder".into());
            ui.set_text_copy_path("Copy Path".into());
            ui.set_text_copy("Copy".into());
            ui.set_text_empty_prompt("Enter search term and press Search".into());
            ui.set_text_no_matches("No matches found for".into());
            ui.set_text_select_file("Select a file from the list".into());
            ui.set_text_recent_folders("Recent folders...".into());
        }
    }
    ui.set_sort_options(Rc::new(VecModel::from(SortOrder::options(lang))).into());
    ui.set_date_options(Rc::new(VecModel::from(DateFilter::options(lang))).into());
    ui.set_size_options(Rc::new(VecModel::from(SizeFilter::options(lang))).into());
}

fn update_cache_ui(ui: &AppWindow, cache: &DocumentCache, lang: AppLanguage) {
    let len = cache.len();
    let bytes = cache.memory_usage_bytes();
    let mb = bytes as f64 / (1024.0 * 1024.0);
    let text = match lang {
        AppLanguage::Finnish => format!("Välimuisti: {} tiedostoa ({:.1} MB)", len, mb),
        AppLanguage::English => format!("Cache: {} files ({:.1} MB)", len, mb),
    };
    ui.set_cache_info(text.into());
    ui.set_cache_has_items(len > 0);
}

fn refresh_results_model(
    ui: &AppWindow,
    results: &[SearchResult],
    selected_idx: Option<usize>,
    lang: AppLanguage,
) {
    let mut items = Vec::with_capacity(results.len());
    for (i, r) in results.iter().enumerate() {
        let (bg, fg) = type_badge_colors(&r.file_type);
        let fname = r.file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let fdir = r.file.parent().map(|p| truncate_path(&p.to_string_lossy(), 36)).unwrap_or_default();
        let date_str = r.modified
            .map(|t| {
                let dt: DateTime<Local> = t.into();
                dt.format("%d.%m.%Y %H:%M").to_string()
            })
            .unwrap_or_default();
        let matches_label = match lang {
            AppLanguage::Finnish => {
                if r.matches.len() >= search::MAX_MATCHES_PER_FILE {
                    format!("{}+ osumaa", search::MAX_MATCHES_PER_FILE)
                } else {
                    format!("{} osumaa", r.matches.len())
                }
            }
            AppLanguage::English => {
                if r.matches.len() >= search::MAX_MATCHES_PER_FILE {
                    format!("{}+ matches", search::MAX_MATCHES_PER_FILE)
                } else {
                    format!("{} matches", r.matches.len())
                }
            }
        };

        items.push(SearchResultItem {
            file_path: r.file.to_string_lossy().to_string().into(),
            file_name: fname.into(),
            dir_path: fdir.into(),
            file_type: r.file_type.clone().into(),
            badge_bg: bg,
            badge_fg: fg,
            date_str: date_str.into(),
            matches_count: r.matches.len() as i32,
            matches_label: matches_label.into(),
            is_selected: selected_idx == Some(i),
        });
    }

    ui.set_results(Rc::new(VecModel::from(items)).into());

    // Update selected preview
    if let Some(idx) = selected_idx {
        if let Some(r) = results.get(idx) {
            ui.set_selected_index(idx as i32);
            let fname = r.file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            ui.set_selected_filename(fname.into());
            ui.set_selected_filepath(r.file.to_string_lossy().to_string().into());

            let header = match lang {
                AppLanguage::Finnish => format!("— {} osumaa", r.matches.len()),
                AppLanguage::English => format!("— {} matches", r.matches.len()),
            };
            ui.set_selected_matches_header(header.into());

            let match_items: Vec<MatchItem> = r.matches.iter().enumerate().map(|(mi, m)| {
                let idx_label = match lang {
                    AppLanguage::Finnish => format!("Osuma #{}", mi + 1),
                    AppLanguage::English => format!("Match #{}", mi + 1),
                };
                MatchItem {
                    index_label: idx_label.into(),
                    context_text: m.context.clone().into(),
                    query: "".into(),
                }
            }).collect();
            ui.set_selected_matches(Rc::new(VecModel::from(match_items)).into());
            return;
        }
    }

    ui.set_selected_index(-1);
    ui.set_selected_filename("".into());
    ui.set_selected_filepath("".into());
    ui.set_selected_matches_header("".into());
    ui.set_selected_matches(Rc::new(VecModel::default()).into());
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ui = AppWindow::new()?;
    let initial_lang = AppLanguage::detect_from_system();
    let initial_dir = get_user_documents_dir().to_string_lossy().to_string();

    ui.set_directory_input(initial_dir.clone().into());
    ui.set_opt_context_size(150.0);
    ui.set_opt_ignore_case(true);
    ui.set_opt_recursive(true);
    ui.set_opt_search_hidden(false);
    ui.set_opt_use_cache(true);
    ui.set_opt_docx(true);
    ui.set_opt_odt(true);
    ui.set_opt_pdf(true);
    ui.set_opt_txt(true);

    update_ui_language(&ui, initial_lang);

    let state = Arc::new(Mutex::new(AppState {
        lang: initial_lang,
        cache: DocumentCache::new(),
        results: Vec::new(),
        errors: Vec::new(),
        selected_result_idx: None,
        recent_directories: vec![initial_dir.clone()],
        current_search_id: 0,
        cancel_flag: None,
        sort_order: SortOrder::DateDesc,
        last_stats: None,
    }));

    {
        let st = state.lock().unwrap();
        update_cache_ui(&ui, &st.cache, initial_lang);
        ui.set_recent_directories(Rc::new(VecModel::from(vec![SharedString::from(&initial_dir)])).into());
    }

    // Callbacks

    // 1. Language change
    let state_lang = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_change_language(move |lang_str| {
        let lang = if lang_str == "English" {
            AppLanguage::English
        } else {
            AppLanguage::Finnish
        };
        if let Some(ui) = ui_weak.upgrade() {
            let mut st = state_lang.lock().unwrap();
            st.lang = lang;
            update_ui_language(&ui, lang);
            update_cache_ui(&ui, &st.cache, lang);
            refresh_results_model(&ui, &st.results, st.selected_result_idx, lang);
        }
    });

    // 2. Browse folder
    let state_browse = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_browse_folder(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let current = resolve_directory_path(ui.get_directory_input().as_str());
            let start = if current.exists() { Some(current.as_path()) } else { None };
            if let Some(path) = pick_folder_dialog(start) {
                let path_str = path.to_string_lossy().to_string();
                ui.set_directory_input(path_str.clone().into());
                ui.set_status_error("".into());
                ui.set_status_info("".into());
                let mut st = state_browse.lock().unwrap();
                if !st.recent_directories.contains(&path_str) {
                    st.recent_directories.insert(0, path_str.clone());
                    let recent_model: Vec<SharedString> = st.recent_directories.iter().map(|s| s.clone().into()).collect();
                    ui.set_recent_directories(Rc::new(VecModel::from(recent_model)).into());
                }
            }
        }
    });

    // 3. Quick folder
    let ui_weak = ui.as_weak();
    ui.on_quick_folder(move |target| {
        if let Some(ui) = ui_weak.upgrade() {
            let path = match target.as_str() {
                "home" => get_user_home_dir(),
                "docs" => get_user_documents_dir(),
                "dl" => get_user_downloads_dir(),
                _ => get_user_documents_dir(),
            };
            ui.set_directory_input(path.to_string_lossy().to_string().into());
            ui.set_status_error("".into());
            ui.set_status_info("".into());
        }
    });

    // 4. Clear cache
    let state_clear_cache = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_clear_cache(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let st = state_clear_cache.lock().unwrap();
            st.cache.clear();
            update_cache_ui(&ui, &st.cache, st.lang);
        }
    });

    // 5. Toggle all file types
    let ui_weak = ui.as_weak();
    ui.on_toggle_all_filetypes(move || {
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_opt_docx(true);
            ui.set_opt_odt(true);
            ui.set_opt_pdf(true);
            ui.set_opt_txt(true);
        }
    });

    // 6. Select result item
    let state_sel = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_select_result(move |idx| {
        if let Some(ui) = ui_weak.upgrade() {
            let mut st = state_sel.lock().unwrap();
            if idx >= 0 && (idx as usize) < st.results.len() {
                st.selected_result_idx = Some(idx as usize);
                let lang = st.lang;
                refresh_results_model(&ui, &st.results, st.selected_result_idx, lang);
            }
        }
    });

    // 7. Open file
    let ui_weak = ui.as_weak();
    ui.on_open_file(move |file_path| {
        let p = PathBuf::from(file_path.as_str());
        if let Err(e) = open::that(&p) {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status_error(format!("Failed to open file: {e}").into());
            }
        }
    });

    // 8. Show in folder
    let ui_weak = ui.as_weak();
    ui.on_show_in_folder(move |file_path| {
        let p = PathBuf::from(file_path.as_str());
        if let Err(e) = show_in_file_manager(&p) {
            if let Some(ui) = ui_weak.upgrade() {
                ui.set_status_error(format!("Failed to open folder: {e}").into());
            }
        }
    });

    // 9. Copy to clipboard
    let state_copy = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_copy_to_clipboard(move |text| {
        copy_to_clipboard(text.as_str());
        if let Some(ui) = ui_weak.upgrade() {
            let st = state_copy.lock().unwrap();
            let msg = match st.lang {
                AppLanguage::Finnish => "Kopioitu leikepöydälle!",
                AppLanguage::English => "Copied to clipboard!",
            };
            ui.set_status_info(msg.into());
        }
    });

    // 10. Save CSV results
    let state_save_csv = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_save_results_csv(move || {
        if let Some(ui) = ui_weak.upgrade() {
            let st = state_save_csv.lock().unwrap();
            if st.results.is_empty() {
                let err = match st.lang {
                    AppLanguage::Finnish => "Ei tallennettavia tuloksia.",
                    AppLanguage::English => "No results to save.",
                };
                ui.set_status_error(err.into());
                return;
            }

            let query = ui.get_query_input().to_string();
            let results = st.results.clone();
            let lang = st.lang;
            drop(st);

            if let Some(save_path) = save_file_dialog("doxsearch-results.csv", "csv") {
                match save_results_csv(&save_path, &query, &results) {
                    Ok(()) => {
                        let msg = match lang {
                            AppLanguage::Finnish => format!("Tulokset tallennettu: {}", save_path.display()),
                            AppLanguage::English => format!("Results saved to: {}", save_path.display()),
                        };
                        ui.set_status_info(msg.into());
                        ui.set_status_error("".into());
                    }
                    Err(e) => {
                        let msg = match lang {
                            AppLanguage::Finnish => format!("Tallennus epäonnistui: {e}"),
                            AppLanguage::English => format!("Failed to save: {e}"),
                        };
                        ui.set_status_error(msg.into());
                    }
                }
            }
        }
    });

    // 11. Sort changed
    let state_sort = state.clone();
    let ui_weak = ui.as_weak();
    ui.on_sort_changed(move |idx| {
        if let Some(ui) = ui_weak.upgrade() {
            let mut st = state_sort.lock().unwrap();
            let sort_order = SortOrder::from_index(idx);
            st.sort_order = sort_order;
            let lang = st.lang;
            sort_results_list(&mut st.results, sort_order);
            refresh_results_model(&ui, &st.results, st.selected_result_idx, lang);
        }
    });

    // 12. Cancel search
    let state_cancel = state.clone();
    let ui_weak = ui.as_weak();
    let cancel_fn = {
        let state_cancel = state_cancel.clone();
        let ui_weak = ui_weak.clone();
        move || {
            if let Some(ui) = ui_weak.upgrade() {
                let mut st = state_cancel.lock().unwrap();
                if let Some(flag) = &st.cancel_flag {
                    flag.store(true, Ordering::Relaxed);
                }
                st.cancel_flag = None;
                st.current_search_id += 1;
                ui.set_is_searching(false);
                ui.set_search_state(if st.results.is_empty() { 0 } else { 2 });
                let msg = match st.lang {
                    AppLanguage::Finnish => "Haku peruutettu.",
                    AppLanguage::English => "Search cancelled.",
                };
                ui.set_status_info(msg.into());
            }
        }
    };

    let cancel_clone = cancel_fn.clone();
    ui.on_cancel_search(move || {
        cancel_clone();
    });

    // 13. Clear search
    let state_clear = state.clone();
    let ui_weak = ui.as_weak();
    let cancel_clone2 = cancel_fn.clone();
    ui.on_clear_search(move || {
        cancel_clone2();
        if let Some(ui) = ui_weak.upgrade() {
            let mut st = state_clear.lock().unwrap();
            st.results.clear();
            st.errors.clear();
            st.selected_result_idx = None;
            st.last_stats = None;
            let lang = st.lang;

            ui.set_query_input("".into());
            ui.set_search_state(0);
            ui.set_is_searching(false);
            ui.set_status_error("".into());
            ui.set_status_info("".into());
            ui.set_stats_summary("".into());
            ui.set_stats_timing("".into());
            ui.set_has_errors(false);
            ui.set_errors(Rc::new(VecModel::default()).into());
            refresh_results_model(&ui, &st.results, None, lang);
        }
    });

    // 14. Start search function
    let state_start = state.clone();
    let ui_weak = ui.as_weak();
    let cancel_clone3 = cancel_fn.clone();
    let run_search = move || {
        if let Some(ui) = ui_weak.upgrade() {
            cancel_clone3();

            let query = ui.get_query_input().to_string();
            let mut st = state_start.lock().unwrap();
            let lang = st.lang;

            if query.trim().is_empty() {
                let err = match lang {
                    AppLanguage::Finnish => "Kirjoita ensin hakusana.",
                    AppLanguage::English => "Please enter a search term first.",
                };
                ui.set_status_error(err.into());
                return;
            }

            let search_docx = ui.get_opt_docx();
            let search_odt = ui.get_opt_odt();
            let search_pdf = ui.get_opt_pdf();
            let search_txt = ui.get_opt_txt();

            if !search_docx && !search_odt && !search_pdf && !search_txt {
                let err = match lang {
                    AppLanguage::Finnish => "Valitse vähintään yksi tiedostotyyppi (DOCX, ODT, PDF, TXT).",
                    AppLanguage::English => "Please select at least one file type (DOCX, ODT, PDF, TXT).",
                };
                ui.set_status_error(err.into());
                return;
            }

            let dir_input = ui.get_directory_input().to_string();
            let dir_path = resolve_directory_path(&dir_input);
            if !dir_path.exists() {
                let err = match lang {
                    AppLanguage::Finnish => format!("Hakemistoa ei löydy: {}", dir_input),
                    AppLanguage::English => format!("Directory not found: {}", dir_input),
                };
                ui.set_status_error(err.into());
                return;
            }

            // Update recent directories
            if !st.recent_directories.contains(&dir_input) {
                st.recent_directories.insert(0, dir_input.clone());
                if st.recent_directories.len() > 6 {
                    st.recent_directories.pop();
                }
                let recent_model: Vec<SharedString> = st.recent_directories.iter().map(|s| s.clone().into()).collect();
                ui.set_recent_directories(Rc::new(VecModel::from(recent_model)).into());
            }

            let date_filter = DateFilter::from_index(ui.get_date_filter_index());
            let size_filter = SizeFilter::from_index(ui.get_size_filter_index());
            let context_size = ui.get_opt_context_size() as usize;

            let opts = SearchOptions {
                query: query.clone(),
                directory: dir_path,
                ignore_case: ui.get_opt_ignore_case(),
                recursive: ui.get_opt_recursive(),
                search_hidden: ui.get_opt_search_hidden(),
                use_cache: ui.get_opt_use_cache(),
                search_docx,
                search_odt,
                search_pdf,
                search_txt,
                context_size,
                max_file_size_mb: size_filter.to_mb(),
                modified_after: date_filter.to_system_time(),
            };

            st.current_search_id += 1;
            let search_id = st.current_search_id;
            let cancel_flag = Arc::new(AtomicBool::new(false));
            st.cancel_flag = Some(cancel_flag.clone());
            st.results.clear();
            st.errors.clear();
            st.selected_result_idx = None;

            ui.set_is_searching(true);
            ui.set_search_state(1);
            ui.set_status_error("".into());
            ui.set_status_info("".into());
            ui.set_stats_summary("".into());
            ui.set_stats_timing("".into());
            ui.set_progress_fraction(0.0);
            let prog_init = match lang {
                AppLanguage::Finnish => "Käydään hakemistoa läpi...",
                AppLanguage::English => "Scanning directory...",
            };
            ui.set_progress_text(prog_init.into());
            ui.set_has_errors(false);
            ui.set_errors(Rc::new(VecModel::default()).into());
            refresh_results_model(&ui, &st.results, None, lang);

            let cache = st.cache.clone();
            let state_worker = state_start.clone();
            let ui_weak_worker = ui.as_weak();

            thread::spawn(move || {
                let ui_prog = ui_weak_worker.clone();
                let ui_match = ui_weak_worker.clone();
                let ui_err = ui_weak_worker.clone();
                let state_match = state_worker.clone();
                let state_err = state_worker.clone();

                let res = search::search_directory(
                    &opts,
                    &cache,
                    Some(&cancel_flag),
                    // On match found:
                    move |result| {
                        let state = state_match.clone();
                        let ui_w = ui_match.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                let mut st = state.lock().unwrap();
                                if st.current_search_id == search_id {
                                    st.results.push(result);
                                    let sort_order = st.sort_order;
                                    if st.results.len() <= 200 {
                                        sort_results_list(&mut st.results, sort_order);
                                    }
                                    if st.selected_result_idx.is_none() {
                                        st.selected_result_idx = Some(0);
                                    }
                                    let lang = st.lang;
                                    refresh_results_model(&ui, &st.results, st.selected_result_idx, lang);
                                }
                            }
                        });
                    },
                    // On error found:
                    move |err| {
                        let state = state_err.clone();
                        let ui_w = ui_err.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                let mut st = state.lock().unwrap();
                                if st.current_search_id == search_id {
                                    st.errors.push(err);
                                    let err_items: Vec<ErrorItem> = st.errors.iter().map(|e| {
                                        let fn_str = e.file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                                        ErrorItem {
                                            file_name: fn_str.into(),
                                            error_message: e.error.clone().into(),
                                        }
                                    }).collect();
                                    let count = st.errors.len();
                                    let header = match st.lang {
                                        AppLanguage::Finnish => format!("Virheet ({count})"),
                                        AppLanguage::English => format!("Errors ({count})"),
                                    };
                                    ui.set_errors_header(header.into());
                                    ui.set_has_errors(true);
                                    ui.set_errors(Rc::new(VecModel::from(err_items)).into());
                                }
                            }
                        });
                    },
                    // On progress:
                    move |processed, total| {
                        let ui_w = ui_prog.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_w.upgrade() {
                                let fraction = if total > 0 { processed as f32 / total as f32 } else { 0.0 };
                                ui.set_progress_fraction(fraction);
                                let p_text = if total > 0 {
                                    format!("{:.0}% ({}/{})", fraction * 100.0, processed, total)
                                } else {
                                    "Scanning...".to_string()
                                };
                                ui.set_progress_text(p_text.into());
                            }
                        });
                    },
                );

                // Done or Error
                let ui_done = ui_weak_worker.clone();
                let state_done = state_worker.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_done.upgrade() {
                        let mut st = state_done.lock().unwrap();
                        if st.current_search_id == search_id {
                            ui.set_is_searching(false);
                            ui.set_search_state(2);
                            let lang = st.lang;
                            let sort_order = st.sort_order;
                            sort_results_list(&mut st.results, sort_order);
                            if !st.results.is_empty() && st.selected_result_idx.is_none() {
                                st.selected_result_idx = Some(0);
                            }
                            refresh_results_model(&ui, &st.results, st.selected_result_idx, lang);
                            update_cache_ui(&ui, &st.cache, lang);

                            match res {
                                Ok(stats) => {
                                    st.last_stats = Some(stats.clone());
                                    let total_matches: usize = st.results.iter().map(|r| r.matches.len()).sum();
                                    let summary = match lang {
                                        AppLanguage::Finnish => {
                                            if total_matches > 0 {
                                                format!("{} osumaa {} tiedostossa", total_matches, st.results.len())
                                            } else {
                                                "Ei osumia".to_string()
                                            }
                                        }
                                        AppLanguage::English => {
                                            if total_matches > 0 {
                                                format!("{} matches in {} files", total_matches, st.results.len())
                                            } else {
                                                "No matches".to_string()
                                            }
                                        }
                                    };
                                    ui.set_stats_summary(summary.into());

                                    let timing = match lang {
                                        AppLanguage::Finnish => {
                                            if stats.cached_count > 0 {
                                                format!("Haettu {} tiedostoa ({} ms) • ⚡ {} välimuistista", stats.total_count, stats.duration.as_millis(), stats.cached_count)
                                            } else {
                                                format!("Haettu {} tiedostoa ({} ms)", stats.total_count, stats.duration.as_millis())
                                            }
                                        }
                                        AppLanguage::English => {
                                            if stats.cached_count > 0 {
                                                format!("Searched {} files ({} ms) • ⚡ {} from cache", stats.total_count, stats.duration.as_millis(), stats.cached_count)
                                            } else {
                                                format!("Searched {} files ({} ms)", stats.total_count, stats.duration.as_millis())
                                            }
                                        }
                                    };
                                    ui.set_stats_timing(timing.into());
                                }
                                Err(e) => {
                                    ui.set_status_error(e.to_string().into());
                                }
                            }
                        }
                    }
                });
            });
        }
    };

    let run_clone = run_search.clone();
    ui.on_start_search(move || {
        run_clone();
    });

    let run_clone2 = run_search.clone();
    ui.on_query_submitted(move || {
        run_clone2();
    });

    let run_clone3 = run_search.clone();
    ui.on_directory_submitted(move || {
        run_clone3();
    });

    ui.run()?;
    Ok(())
}
