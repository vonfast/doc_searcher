mod search;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use egui::{Color32, FontId, RichText, Vec2};
use search::{DocumentCache, SearchError, SearchOptions, SearchResult, SearchStats};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::thread;

const BLUE_DARK: Color32 = Color32::from_rgb(30, 58, 138);
const BLUE_MED: Color32 = Color32::from_rgb(59, 130, 246);
const GREEN: Color32 = Color32::from_rgb(34, 197, 94);
const ORANGE: Color32 = Color32::from_rgb(249, 115, 22);
const PURPLE: Color32 = Color32::from_rgb(147, 51, 234);
const RED_ACCENT: Color32 = Color32::from_rgb(239, 68, 68);
const GRAY_BORDER: Color32 = Color32::from_rgb(203, 213, 225);
const TEXT_DARK: Color32 = Color32::from_rgb(15, 23, 42);
const TEXT_MED: Color32 = Color32::from_rgb(71, 85, 105);

enum SearchMessage {
    Progress {
        search_id: usize,
        processed: usize,
        total: usize,
    },
    MatchFound {
        search_id: usize,
        result: SearchResult,
    },
    ErrorFound {
        search_id: usize,
        error: SearchError,
    },
    Done {
        search_id: usize,
        cached_count: usize,
        total_count: usize,
        duration: std::time::Duration,
    },
    Error {
        search_id: usize,
        message: String,
    },
    UiError(String),
}

use chrono::{DateTime, Local};

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
    fn label(&self, lang: AppLanguage) -> &'static str {
        match lang {
            AppLanguage::Finnish => match self {
                SortOrder::DateDesc => "Päivämäärä (Uusin ensin)",
                SortOrder::DateAsc => "Päivämäärä (Vanhin ensin)",
                SortOrder::Name => "Nimi (A-Z)",
                SortOrder::Matches => "Osumien määrä",
            },
            AppLanguage::English => match self {
                SortOrder::DateDesc => "Date (Newest first)",
                SortOrder::DateAsc => "Date (Oldest first)",
                SortOrder::Name => "Name (A-Z)",
                SortOrder::Matches => "Match Count",
            },
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
    fn label(&self, lang: AppLanguage) -> &'static str {
        match lang {
            AppLanguage::Finnish => match self {
                DateFilter::All => "Kaikki ajat",
                DateFilter::Last24Hours => "Viimeiset 24 tuntia",
                DateFilter::Last7Days => "Viimeiset 7 päivää",
                DateFilter::Last30Days => "Viimeiset 30 päivää",
                DateFilter::LastYear => "Viimeinen vuosi",
            },
            AppLanguage::English => match self {
                DateFilter::All => "All time",
                DateFilter::Last24Hours => "Last 24 hours",
                DateFilter::Last7Days => "Last 7 days",
                DateFilter::Last30Days => "Last 30 days",
                DateFilter::LastYear => "Last year",
            },
        }
    }

    fn to_system_time(self) -> Option<std::time::SystemTime> {
        let now = std::time::SystemTime::now();
        match self {
            DateFilter::All => None,
            DateFilter::Last24Hours => now.checked_sub(std::time::Duration::from_secs(24 * 3600)),
            DateFilter::Last7Days => now.checked_sub(std::time::Duration::from_secs(7 * 24 * 3600)),
            DateFilter::Last30Days => {
                now.checked_sub(std::time::Duration::from_secs(30 * 24 * 3600))
            }
            DateFilter::LastYear => {
                now.checked_sub(std::time::Duration::from_secs(365 * 24 * 3600))
            }
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
    fn label(&self, lang: AppLanguage) -> &'static str {
        match lang {
            AppLanguage::Finnish => match self {
                SizeFilter::NoLimit => "Ei kokorajoitusta",
                SizeFilter::Max10MB => "Enintään 10 MB",
                SizeFilter::Max50MB => "Enintään 50 MB",
                SizeFilter::Max100MB => "Enintään 100 MB",
            },
            AppLanguage::English => match self {
                SizeFilter::NoLimit => "No size limit",
                SizeFilter::Max10MB => "Max 10 MB",
                SizeFilter::Max50MB => "Max 50 MB",
                SizeFilter::Max100MB => "Max 100 MB",
            },
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

#[derive(Debug, PartialEq, Clone, Copy)]
enum SearchState {
    Idle,
    Searching,
    Done,
}

pub fn os_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "macOS"
    } else if cfg!(target_os = "windows") {
        "Windows"
    } else if cfg!(target_os = "linux") {
        "Linux"
    } else {
        "Unix"
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
        // 1. Check default mime handler for inode/directory
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

        // 2. Check XDG_CURRENT_DESKTOP / DESKTOP_SESSION
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

pub fn pick_folder_dialog(start_dir: Option<&std::path::Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new();
    if let Some(dir) = start_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_folder()
}

pub fn save_file_dialog(default_name: &str, filter_ext: &str) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_file_name(default_name)
        .add_filter("CSV files", &[filter_ext])
        .save_file()
}

#[cfg(target_os = "linux")]
fn show_in_file_manager_dbus(uri: &str, is_dir: bool) -> Result<(), String> {
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let uri_str = uri.to_string();

    let spawn_res = thread::Builder::new()
        .name("dbus-fm-call".to_string())
        .spawn(move || {
            let conn = match zbus::blocking::Connection::session() {
                Ok(c) => c,
                Err(e) => {
                    let _ = done_tx.send(Err(format!("D-Bus session connect error: {e}")));
                    return;
                }
            };

            let methods = if is_dir {
                ["ShowFolders", "ShowItems"]
            } else {
                ["ShowItems", "ShowFolders"]
            };

            for method in methods {
                let uris = [uri_str.as_str()];
                if conn
                    .call_method(
                        Some("org.freedesktop.FileManager1"),
                        "/org/freedesktop/FileManager1",
                        Some("org.freedesktop.FileManager1"),
                        method,
                        &(uris.as_slice(), ""),
                    )
                    .is_ok()
                {
                    let _ = done_tx.send(Ok(()));
                    return;
                }
            }

            let _ = done_tx.send(Err("D-Bus FileManager1 call failed".to_string()));
        });

    if spawn_res.is_err() {
        return Err("Failed to spawn D-Bus thread".to_string());
    }

    match done_rx.recv_timeout(std::time::Duration::from_millis(1500)) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("D-Bus FileManager1 timed out".to_string()),
    }
}

pub fn show_in_file_manager(path: &std::path::Path) -> Result<(), String> {
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
        let uri = path_to_file_uri(&canonical);
        let is_dir = canonical.is_dir();

        // 1. Ensisijainen: Kevyt D-Bus IPC (org.freedesktop.FileManager1) zbus-kirjaston kautta.
        // Ei luo uutta prosessia (kuten dbus-send tai dolphin) ja käyttää erillistä säiettä
        // sekä 1.5 sekunnin aikakatkaisua (timeout), jotta UI ei voi koskaan jäätyä.
        if show_in_file_manager_dbus(&uri, is_dir).is_ok() {
            return Ok(());
        }

        // 2. Varatapa: Avaa kansio järjestelmän oletuskäsittelijällä (xdg-open via open::that)
        let target = if is_dir {
            &canonical
        } else {
            canonical.parent().unwrap_or(&canonical)
        };
        open::that(target).map_err(|e| format!("Failed to open folder: {e}"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    {
        let parent = path.parent().unwrap_or(path);
        open::that(parent).map_err(|e| format!("Failed to open folder: {e}"))
    }
}

/// Converts a local filesystem path into an RFC 3986 compliant file URI with percent-encoding for special characters.
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
    } else if let Some(stripped) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
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

struct DoXsearchApp {
    opts: SearchOptions,
    directory_input: String,
    state: SearchState,
    results: Vec<SearchResult>,
    errors: Vec<SearchError>,
    error: Option<String>,
    status_info: Option<String>,
    progress_count: (usize, usize),
    selected_result: Option<usize>,
    sort_order: SortOrder,
    date_filter: DateFilter,
    size_filter: SizeFilter,
    recent_directories: Vec<String>,
    cache: DocumentCache,
    last_search_stats: Option<SearchStats>,
    lang: AppLanguage,
    current_search_id: usize,
    cancel_flag: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    tx: Sender<SearchMessage>,
    rx: Receiver<SearchMessage>,
}

impl DoXsearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let (tx, rx) = crossbeam_channel::unbounded();
        let initial_dir = get_user_documents_dir().to_string_lossy().to_string();
        Self {
            directory_input: initial_dir.clone(),
            opts: SearchOptions {
                directory: PathBuf::from(&initial_dir),
                ..Default::default()
            },
            state: SearchState::Idle,
            results: Vec::new(),
            errors: Vec::new(),
            error: None,
            status_info: None,
            progress_count: (0, 0),
            selected_result: None,
            sort_order: SortOrder::DateDesc,
            date_filter: DateFilter::All,
            size_filter: SizeFilter::NoLimit,
            recent_directories: vec![initial_dir],
            cache: DocumentCache::new(),
            last_search_stats: None,
            lang: AppLanguage::detect_from_system(),
            current_search_id: 0,
            cancel_flag: None,
            tx,
            rx,
        }
    }

    fn cancel_search(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        self.cancel_flag = None;
        self.current_search_id += 1;
        if self.state == SearchState::Searching {
            self.state = if self.results.is_empty() {
                SearchState::Idle
            } else {
                SearchState::Done
            };
            self.progress_count = (0, 0);
            self.status_info = Some(if self.lang == AppLanguage::Finnish {
                "Haku peruutettu.".to_string()
            } else {
                "Search cancelled.".to_string()
            });
        }
    }

    fn sort_results(&mut self) {
        let selected_path = self
            .selected_result
            .and_then(|idx| self.results.get(idx).map(|r| r.file.clone()));

        match self.sort_order {
            SortOrder::DateDesc => {
                self.results.sort_by(|a, b| match (b.modified, a.modified) {
                    (Some(tb), Some(ta)) => tb.cmp(&ta).then_with(|| a.file.cmp(&b.file)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.file.cmp(&b.file),
                });
            }
            SortOrder::DateAsc => {
                self.results.sort_by(|a, b| match (a.modified, b.modified) {
                    (Some(ta), Some(tb)) => ta.cmp(&tb).then_with(|| a.file.cmp(&b.file)),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => a.file.cmp(&b.file),
                });
            }
            SortOrder::Name => {
                self.results.sort_by(|a, b| {
                    let name_a = a
                        .file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_lowercase());
                    let name_b = b
                        .file
                        .file_name()
                        .map(|n| n.to_string_lossy().to_lowercase());
                    name_a.cmp(&name_b).then_with(|| a.file.cmp(&b.file))
                });
            }
            SortOrder::Matches => {
                self.results.sort_by(|a, b| {
                    b.matches
                        .len()
                        .cmp(&a.matches.len())
                        .then_with(|| a.file.cmp(&b.file))
                });
            }
        }

        if let Some(path) = selected_path {
            self.selected_result = self.results.iter().position(|r| r.file == path);
        }
    }

    fn start_search(&mut self) {
        // Cancel any existing search before starting a new one
        self.cancel_search();

        if self.opts.query.trim().is_empty() {
            self.error = Some(if self.lang == AppLanguage::Finnish {
                "Kirjoita ensin hakusana.".to_string()
            } else {
                "Please enter a search term first.".to_string()
            });
            self.status_info = None;
            return;
        }
        if !self.opts.search_docx
            && !self.opts.search_odt
            && !self.opts.search_pdf
            && !self.opts.search_txt
        {
            self.error = Some(if self.lang == AppLanguage::Finnish {
                "Valitse vähintään yksi tiedostotyyppi (DOCX, ODT, PDF, TXT).".to_string()
            } else {
                "Please select at least one file type (DOCX, ODT, PDF, TXT).".to_string()
            });
            self.status_info = None;
            return;
        }

        self.opts.directory = resolve_directory_path(&self.directory_input);
        if !self.opts.directory.exists() {
            self.error = Some(if self.lang == AppLanguage::Finnish {
                format!("Hakemistoa ei löydy: {}", self.directory_input)
            } else {
                format!("Directory not found: {}", self.directory_input)
            });
            self.status_info = None;
            return;
        }

        // Add to recent directories (MRU order)
        let dir_str = self.directory_input.clone();
        if let Some(pos) = self.recent_directories.iter().position(|d| d == &dir_str) {
            self.recent_directories.remove(pos);
        }
        self.recent_directories.insert(0, dir_str);
        if self.recent_directories.len() > 6 {
            self.recent_directories.pop();
        }

        self.opts.modified_after = self.date_filter.to_system_time();
        self.opts.max_file_size_mb = self.size_filter.to_mb();

        self.current_search_id += 1;
        let search_id = self.current_search_id;
        let cancel_flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.cancel_flag = Some(cancel_flag.clone());

        self.state = SearchState::Searching;
        self.results.clear();
        self.errors.clear();
        self.error = None;
        self.status_info = None;
        self.selected_result = None;
        self.progress_count = (0, 0);

        let opts = self.opts.clone();
        let cache = self.cache.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let tx_match = tx.clone();
            let tx_err = tx.clone();
            let tx_prog = tx.clone();
            match search::search_directory(
                &opts,
                &cache,
                Some(&cancel_flag),
                move |result| {
                    let _ = tx_match.send(SearchMessage::MatchFound { search_id, result });
                },
                move |error| {
                    let _ = tx_err.send(SearchMessage::ErrorFound { search_id, error });
                },
                move |processed, total| {
                    let _ = tx_prog.send(SearchMessage::Progress {
                        search_id,
                        processed,
                        total,
                    });
                },
            ) {
                Ok(stats) => {
                    let _ = tx.send(SearchMessage::Done {
                        search_id,
                        cached_count: stats.cached_count,
                        total_count: stats.total_count,
                        duration: stats.duration,
                    });
                }
                Err(e) => {
                    let _ = tx.send(SearchMessage::Error {
                        search_id,
                        message: e.to_string(),
                    });
                }
            }
        });
    }

    fn clear_search(&mut self) {
        self.cancel_search();
        self.current_search_id += 1;
        self.results.clear();
        self.errors.clear();
        self.opts.query.clear();
        self.state = SearchState::Idle;
        self.error = None;
        self.status_info = None;
        self.progress_count = (0, 0);
        self.selected_result = None;
        self.last_search_stats = None;
    }

    fn poll_messages(&mut self, ctx: &egui::Context) {
        let mut got_msg = false;
        let mut new_matches = false;
        while let Ok(msg) = self.rx.try_recv() {
            got_msg = true;
            match msg {
                SearchMessage::Progress {
                    search_id,
                    processed,
                    total,
                } => {
                    if search_id == self.current_search_id {
                        self.progress_count = (processed, total);
                    }
                }
                SearchMessage::MatchFound { search_id, result } => {
                    if search_id == self.current_search_id {
                        self.results.push(result);
                        new_matches = true;
                        if self.selected_result.is_none() {
                            self.selected_result = Some(0);
                        }
                    }
                }
                SearchMessage::ErrorFound { search_id, error } => {
                    if search_id == self.current_search_id {
                        self.errors.push(error);
                    }
                }
                SearchMessage::Done {
                    search_id,
                    cached_count,
                    total_count,
                    duration,
                } => {
                    if search_id == self.current_search_id {
                        self.sort_results();
                        self.state = SearchState::Done;
                        self.progress_count = (0, 0);
                        self.last_search_stats = Some(SearchStats {
                            cached_count,
                            total_count,
                            duration,
                        });
                        if !self.results.is_empty() && self.selected_result.is_none() {
                            self.selected_result = Some(0);
                        }
                    }
                }
                SearchMessage::Error { search_id, message } => {
                    if search_id == self.current_search_id {
                        self.error = Some(message);
                        self.status_info = None;
                        self.state = SearchState::Idle;
                    }
                }
                SearchMessage::UiError(message) => {
                    self.error = Some(message);
                    self.status_info = None;
                }
            }
        }
        if new_matches && self.results.len() <= 200 {
            self.sort_results();
        }
        // Ensure selected_result is valid and within bounds
        if let Some(sel) = self.selected_result {
            if sel >= self.results.len() {
                self.selected_result = if self.results.is_empty() {
                    None
                } else {
                    Some(0)
                };
            }
        }
        if got_msg {
            ctx.request_repaint();
        }
    }

    fn open_in_file_manager_async(&self, path: &Path) {
        let path = path.to_path_buf();
        let tx = self.tx.clone();
        thread::Builder::new()
            .name("show-in-fm".to_string())
            .spawn(move || {
                if let Err(e) = show_in_file_manager(&path) {
                    let _ = tx.send(SearchMessage::UiError(e));
                }
            })
            .ok();
    }

    fn open_file_async(&self, path: &Path) {
        let path = path.to_path_buf();
        let tx = self.tx.clone();
        thread::Builder::new()
            .name("open-file".to_string())
            .spawn(move || {
                if let Err(e) = open::that(&path) {
                    let _ = tx.send(SearchMessage::UiError(format!("Failed to open file: {e}")));
                }
            })
            .ok();
    }

    fn total_matches(&self) -> usize {
        self.results.iter().map(|r| r.matches.len()).sum()
    }

    fn save_results(&mut self) {
        if self.results.is_empty() {
            self.error = Some(if self.lang == AppLanguage::Finnish {
                "Ei tallennettavia tuloksia.".to_string()
            } else {
                "There are no results to save.".to_string()
            });
            self.status_info = None;
            return;
        }

        let Some(path) = save_file_dialog("doxsearch-results.csv", "csv") else {
            return;
        };

        match save_results_csv(&path, &self.opts.query, &self.results) {
            Ok(()) => {
                self.status_info = Some(if self.lang == AppLanguage::Finnish {
                    format!("Tulokset tallennettu tiedostoon {}", path.display())
                } else {
                    format!("Results saved to {}", path.display())
                });
                self.error = None;
            }
            Err(e) => {
                self.error = Some(if self.lang == AppLanguage::Finnish {
                    format!("Tulosten tallennus epäonnistui: {e}")
                } else {
                    format!("Failed to save results: {e}")
                });
                self.status_info = None;
            }
        }
    }
}

fn csv_field(value: &str) -> String {
    let escaped = value.replace('"', "\"\"");
    format!("\"{escaped}\"")
}

fn save_results_csv(
    path: &std::path::Path,
    query: &str,
    results: &[SearchResult],
) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut writer = std::io::BufWriter::new(file);
    // Write UTF-8 BOM so Microsoft Excel on Windows parses UTF-8 correctly
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

impl eframe::App for DoXsearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages(ctx);
        if self.state == SearchState::Searching {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // Top Bar
        egui::TopBottomPanel::top("topbar")
            .exact_height(52.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.add_space(16.0);
                    ui.label(
                        RichText::new("DoXsearch")
                            .font(FontId::proportional(22.0))
                            .color(BLUE_DARK)
                            .strong(),
                    );
                    ui.add_space(12.0);
                    let subtitle = if self.lang == AppLanguage::Finnish {
                        "| Etsi tekstiä DOCX-, ODT-, PDF- ja tekstitiedostoista"
                    } else {
                        "| Search text in DOCX, ODT, PDF and TXT files"
                    };
                    ui.label(
                        RichText::new(subtitle)
                            .font(FontId::proportional(13.0))
                            .color(TEXT_MED),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add_space(16.0);
                        egui::ComboBox::from_id_source("lang_selector_combo")
                            .selected_text(
                                RichText::new(self.lang.label()).font(FontId::proportional(12.0)),
                            )
                            .show_ui(ui, |ui| {
                                ui.selectable_value(
                                    &mut self.lang,
                                    AppLanguage::Finnish,
                                    AppLanguage::Finnish.label(),
                                );
                                ui.selectable_value(
                                    &mut self.lang,
                                    AppLanguage::English,
                                    AppLanguage::English.label(),
                                );
                            });
                    });
                });
            });

        // Search panel (left)
        egui::SidePanel::left("search_panel")
            .exact_width(280.0)
            .resizable(false)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            let dir_label = if self.lang == AppLanguage::Finnish { "Hakemisto" } else { "Directory" };
                            ui.label(RichText::new(dir_label).color(TEXT_MED).strong());
                            ui.add_space(3.0);
                            let dir_resp = ui.add(egui::TextEdit::singleline(&mut self.directory_input)
                                .desired_width(240.0)
                                .hint_text("/home/user/docs")
                                .font(FontId::monospace(11.0)));
                            if dir_resp.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && self.state != SearchState::Searching
                            {
                                self.start_search();
                            }
                            ui.add_space(3.0);
                            ui.horizontal(|ui| {
                                let browse_label = if self.lang == AppLanguage::Finnish { "Selaa..." } else { "Browse..." };
                                if ui.button(browse_label).clicked() {
                                    let current_dir = resolve_directory_path(&self.directory_input);
                                    let start = if current_dir.exists() { Some(current_dir.as_path()) } else { None };
                                    if let Some(path) = pick_folder_dialog(start) {
                                        self.directory_input = path.to_string_lossy().to_string();
                                        self.error = None;
                                        self.status_info = None;
                                    }
                                }

                                let docs_dir = get_user_documents_dir();
                                let docs_label = docs_dir.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| if self.lang == AppLanguage::Finnish { "Asiakirjat".to_string() } else { "Docs".to_string() });
                                let dl_dir = get_user_downloads_dir();
                                let dl_label = dl_dir.file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| if self.lang == AppLanguage::Finnish { "Lataukset".to_string() } else { "Downloads".to_string() });
                                let home_label = if self.lang == AppLanguage::Finnish { "~ Koti" } else { "~ Home" };

                                if ui.small_button(home_label).clicked() {
                                    self.directory_input = get_user_home_dir().to_string_lossy().to_string();
                                    self.error = None;
                                    self.status_info = None;
                                }
                                if ui.small_button(&docs_label).clicked() {
                                    self.directory_input = docs_dir.to_string_lossy().to_string();
                                    self.error = None;
                                    self.status_info = None;
                                }
                                if ui.small_button(&dl_label).clicked() {
                                    self.directory_input = dl_dir.to_string_lossy().to_string();
                                    self.error = None;
                                    self.status_info = None;
                                }
                            });

                            if !self.recent_directories.is_empty() {
                                ui.add_space(2.0);
                                let recent_label = if self.lang == AppLanguage::Finnish { "Viimeisimmät kansiot..." } else { "Recent folders..." };
                                egui::ComboBox::from_id_source("recent_dirs_combo")
                                    .selected_text(RichText::new(recent_label).font(FontId::proportional(11.0)))
                                    .show_ui(ui, |ui| {
                                        for dir in &self.recent_directories {
                                            let label = truncate_path(dir, 32);
                                            if ui.selectable_value(&mut self.directory_input, dir.clone(), label).clicked() {
                                                self.error = None;
                                                self.status_info = None;
                                            }
                                        }
                                    });
                            }

                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(10.0);

                            let query_label = if self.lang == AppLanguage::Finnish { "Hakusana" } else { "Search term" };
                            let query_hint = if self.lang == AppLanguage::Finnish { "Kirjoita hakusana..." } else { "Type search term..." };
                            ui.label(RichText::new(query_label).color(TEXT_MED).strong());
                            ui.add_space(3.0);
                            let qr = ui.add(egui::TextEdit::singleline(&mut self.opts.query)
                                .desired_width(240.0)
                                .hint_text(query_hint)
                                .font(FontId::proportional(14.0)));
                            if qr.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))
                                && self.state != SearchState::Searching
                            {
                                self.start_search();
                            }
                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(10.0);

                            let settings_label = if self.lang == AppLanguage::Finnish { "Asetukset" } else { "Settings" };
                            let case_label = if self.lang == AppLanguage::Finnish { "Älä huomioi kirjainkokoa" } else { "Ignore case" };
                            let rec_label = if self.lang == AppLanguage::Finnish { "Rekursiivinen haku" } else { "Recursive search" };
                            let hidden_label = if self.lang == AppLanguage::Finnish { "Hae piilokansioista" } else { "Search hidden folders" };
                            let cache_cb_label = if self.lang == AppLanguage::Finnish { "Käytä välimuistia" } else { "Use memory cache" };

                            ui.label(RichText::new(settings_label).color(TEXT_MED).strong());
                            ui.add_space(4.0);
                            ui.checkbox(&mut self.opts.ignore_case, case_label);
                            ui.checkbox(&mut self.opts.recursive, rec_label);
                            ui.checkbox(&mut self.opts.search_hidden, hidden_label);
                            ui.checkbox(&mut self.opts.use_cache, cache_cb_label);
                            ui.add_space(4.0);

                            let cache_len = self.cache.len();
                            let cache_bytes = self.cache.memory_usage_bytes();
                            let cache_mb = cache_bytes as f64 / (1024.0 * 1024.0);
                            let cache_info = if self.lang == AppLanguage::Finnish {
                                format!("Välimuisti: {} tiedostoa ({:.1} MB)", cache_len, cache_mb)
                            } else {
                                format!("Cache: {} files ({:.1} MB)", cache_len, cache_mb)
                            };
                            let clear_cache_label = if self.lang == AppLanguage::Finnish { "Tyhjennä" } else { "Clear" };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(cache_info)
                                    .font(FontId::proportional(11.0)).color(TEXT_MED));
                                if cache_len > 0 && ui.small_button(RichText::new(clear_cache_label).color(RED_ACCENT)).clicked() {
                                    self.cache.clear();
                                }
                            });
                            ui.add_space(6.0);

                            let file_types_label = if self.lang == AppLanguage::Finnish { "Tiedostotyypit:" } else { "File types:" };
                            let all_btn_label = if self.lang == AppLanguage::Finnish { "Kaikki" } else { "All" };
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(file_types_label).color(TEXT_MED));
                                let all_selected = self.opts.search_docx && self.opts.search_odt && self.opts.search_pdf && self.opts.search_txt;
                                if !all_selected && ui.small_button(RichText::new(all_btn_label).font(FontId::proportional(11.0)).color(BLUE_MED)).clicked() {
                                    self.opts.search_docx = true;
                                    self.opts.search_odt = true;
                                    self.opts.search_pdf = true;
                                    self.opts.search_txt = true;
                                }
                            });
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                ftbtn(ui, &mut self.opts.search_docx, "DOCX", file_type_color("DOCX"));
                                ftbtn(ui, &mut self.opts.search_odt,  "ODT",  file_type_color("ODT"));
                                ftbtn(ui, &mut self.opts.search_pdf,  "PDF",  file_type_color("PDF"));
                                ftbtn(ui, &mut self.opts.search_txt,  "TXT",  file_type_color("TXT"));
                            });
                            ui.add_space(6.0);

                            let date_range_label = if self.lang == AppLanguage::Finnish { "Aikarajaus:" } else { "Date range:" };
                            ui.label(RichText::new(date_range_label).color(TEXT_MED));
                            egui::ComboBox::from_id_source("date_filter_combo")
                                .selected_text(RichText::new(self.date_filter.label(self.lang)).font(FontId::proportional(12.0)))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.date_filter, DateFilter::All, DateFilter::All.label(self.lang));
                                    ui.selectable_value(&mut self.date_filter, DateFilter::Last24Hours, DateFilter::Last24Hours.label(self.lang));
                                    ui.selectable_value(&mut self.date_filter, DateFilter::Last7Days, DateFilter::Last7Days.label(self.lang));
                                    ui.selectable_value(&mut self.date_filter, DateFilter::Last30Days, DateFilter::Last30Days.label(self.lang));
                                    ui.selectable_value(&mut self.date_filter, DateFilter::LastYear, DateFilter::LastYear.label(self.lang));
                                });
                            ui.add_space(6.0);

                            let max_size_label = if self.lang == AppLanguage::Finnish { "Maksimikoko:" } else { "Max file size:" };
                            ui.label(RichText::new(max_size_label).color(TEXT_MED));
                            egui::ComboBox::from_id_source("size_filter_combo")
                                .selected_text(RichText::new(self.size_filter.label(self.lang)).font(FontId::proportional(12.0)))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.size_filter, SizeFilter::NoLimit, SizeFilter::NoLimit.label(self.lang));
                                    ui.selectable_value(&mut self.size_filter, SizeFilter::Max10MB, SizeFilter::Max10MB.label(self.lang));
                                    ui.selectable_value(&mut self.size_filter, SizeFilter::Max50MB, SizeFilter::Max50MB.label(self.lang));
                                    ui.selectable_value(&mut self.size_filter, SizeFilter::Max100MB, SizeFilter::Max100MB.label(self.lang));
                                });
                            ui.add_space(6.0);

                            let context_label = if self.lang == AppLanguage::Finnish { "Konteksti:" } else { "Context:" };
                            ui.label(RichText::new(context_label).color(TEXT_MED));
                            ui.add(egui::Slider::new(&mut self.opts.context_size, 50..=500).text(""));
                            ui.add_space(12.0);

                            let searching = self.state == SearchState::Searching;
                            let search_btn_text = if searching {
                                if self.lang == AppLanguage::Finnish { "Haetaan..." } else { "Searching..." }
                            } else {
                                if self.lang == AppLanguage::Finnish { "Hae" } else { "Search" }
                            };
                            let cancel_btn_text = if self.lang == AppLanguage::Finnish { "Peruuta" } else { "Cancel" };
                            let clear_btn_text = if self.lang == AppLanguage::Finnish { "Tyhjennä" } else { "Clear" };

                            ui.horizontal(|ui| {
                                if searching {
                                    if ui.add(
                                        egui::Button::new(
                                            RichText::new(cancel_btn_text)
                                                .font(FontId::proportional(15.0)).strong().color(Color32::WHITE)
                                        ).fill(RED_ACCENT).min_size(Vec2::new(115.0, 36.0))
                                    ).clicked() {
                                        self.cancel_search();
                                    }
                                } else {
                                    if ui.add(
                                        egui::Button::new(
                                            RichText::new(search_btn_text)
                                                .font(FontId::proportional(15.0)).strong()
                                        ).min_size(Vec2::new(115.0, 36.0))
                                    ).clicked() {
                                        self.start_search();
                                    }
                                }

                                if ui.add(
                                    egui::Button::new(
                                        RichText::new(clear_btn_text)
                                            .font(FontId::proportional(15.0))
                                    ).min_size(Vec2::new(115.0, 36.0))
                                ).clicked() {
                                    self.clear_search();
                                }
                            });

                            if searching {
                                ui.add_space(8.0);
                                let (processed, total) = self.progress_count;
                                let fraction = if total > 0 { processed as f32 / total as f32 } else { 0.0 };
                                let text = if total > 0 {
                                    format!("{:.0}% ({} / {})", fraction * 100.0, processed, total)
                                } else if self.lang == AppLanguage::Finnish {
                                    "Käydään hakemistoa läpi...".to_string()
                                } else {
                                    "Scanning directory...".to_string()
                                };
                                ui.add(egui::ProgressBar::new(fraction).text(text).animate(true));
                            }

                            if let Some(info) = &self.status_info {
                                ui.add_space(6.0);
                                ui.colored_label(Color32::from_rgb(22, 101, 52), info);
                            }

                            if let Some(err) = &self.error {
                                ui.add_space(6.0);
                                ui.colored_label(Color32::RED, err);
                            }

                            if self.state == SearchState::Done {
                                ui.add_space(12.0);
                                let save_btn_text = if self.lang == AppLanguage::Finnish { "Tallenna tulokset..." } else { "Save results..." };
                                if ui.button(save_btn_text).clicked() {
                                    self.save_results();
                                }
                                let total = self.total_matches();
                                if total > 0 {
                                    let matches_summary = if self.lang == AppLanguage::Finnish {
                                        format!("{} osumaa {} tiedostossa", total, self.results.len())
                                    } else {
                                        format!("{} matches in {} files", total, self.results.len())
                                    };
                                    ui.label(RichText::new(matches_summary)
                                        .color(Color32::DARK_GREEN).strong());
                                } else {
                                    let no_matches_text = if self.lang == AppLanguage::Finnish { "Ei osumia" } else { "No matches found" };
                                    ui.label(RichText::new(no_matches_text).color(TEXT_MED));
                                }

                                if let Some(stats) = &self.last_search_stats {
                                    let stats_text = if self.lang == AppLanguage::Finnish {
                                        if stats.cached_count > 0 {
                                            format!("Haettu {} tiedostoa ({} ms)\n⚡ {} luettu välimuistista", stats.total_count, stats.duration.as_millis(), stats.cached_count)
                                        } else {
                                            format!("Haettu {} tiedostoa ({} ms)", stats.total_count, stats.duration.as_millis())
                                        }
                                    } else {
                                        if stats.cached_count > 0 {
                                            format!("Searched {} files ({} ms)\n⚡ {} from cache", stats.total_count, stats.duration.as_millis(), stats.cached_count)
                                        } else {
                                            format!("Searched {} files ({} ms)", stats.total_count, stats.duration.as_millis())
                                        }
                                    };
                                    ui.add_space(2.0);
                                    ui.label(RichText::new(stats_text).font(FontId::proportional(11.0)).color(TEXT_MED));
                                }

                                if !self.errors.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    let errors_label = if self.lang == AppLanguage::Finnish {
                                        format!("Virheet ({})", self.errors.len())
                                    } else {
                                        format!("Errors ({})", self.errors.len())
                                    };
                                    egui::CollapsingHeader::new(
                                        RichText::new(errors_label).color(RED_ACCENT).strong()
                                    )
                                    .default_open(true)
                                    .show(ui, |ui| {
                                        for err in &self.errors {
                                            let fname = err.file.file_name()
                                                .unwrap_or_default().to_string_lossy();
                                            ui.label(RichText::new(format!("• {}: {}", fname, err.error))
                                                .font(FontId::proportional(11.0))
                                                .color(RED_ACCENT));
                                        }
                                    });
                                }
                            }
                        });
                    });
                });
            });

        // File list panel (center)
        if !self.results.is_empty() {
            let mut new_sel = self.selected_result;

            egui::SidePanel::left("file_list_panel")
                .exact_width(280.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        let files_label = if self.lang == AppLanguage::Finnish {
                            format!("Tiedostot ({})", self.results.len())
                        } else {
                            format!("Files ({})", self.results.len())
                        };
                        ui.label(
                            RichText::new(files_label)
                                .font(FontId::proportional(12.0))
                                .color(TEXT_MED)
                                .strong(),
                        );

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let prev_sort = self.sort_order;
                            egui::ComboBox::from_id_source("sort_order_combo")
                                .selected_text(
                                    RichText::new(self.sort_order.label(self.lang))
                                        .font(FontId::proportional(11.0)),
                                )
                                .show_ui(ui, |ui| {
                                    for &order in &[
                                        SortOrder::DateDesc,
                                        SortOrder::DateAsc,
                                        SortOrder::Name,
                                        SortOrder::Matches,
                                    ] {
                                        ui.selectable_value(
                                            &mut self.sort_order,
                                            order,
                                            order.label(self.lang),
                                        );
                                    }
                                });
                            if self.sort_order != prev_sort {
                                self.sort_results();
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .id_source("file_list_scroll")
                        .show_rows(ui, 88.0, self.results.len(), |ui, row_range| {
                            for ri in row_range {
                                let result = &self.results[ri];
                                let is_sel = self.selected_result == Some(ri);
                                let fname = result
                                    .file
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy();
                                let fdir = result
                                    .file
                                    .parent()
                                    .map(|p| truncate_path(&p.to_string_lossy(), 24))
                                    .unwrap_or_default();

                                let type_color = file_type_color(&result.file_type);

                                let date_str = result
                                    .modified
                                    .map(|sys_time| {
                                        let dt: DateTime<Local> = sys_time.into();
                                        dt.format("%d.%m.%Y %H:%M").to_string()
                                    })
                                    .unwrap_or_default();

                                ui.add_space(2.0);

                                // Filename — clickable row
                                let label_text = format!("[{}] {}", result.file_type, fname);
                                let sel_label = egui::SelectableLabel::new(
                                    is_sel,
                                    RichText::new(&label_text)
                                        .font(FontId::proportional(13.0))
                                        .color(if is_sel { BLUE_DARK } else { type_color })
                                        .strong(),
                                );
                                if ui.add(sel_label).clicked() {
                                    new_sel = Some(ri);
                                }
                                ui.add_space(2.0);

                                // Directory path + Date + Matches + open button
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    ui.label(
                                        RichText::new(&fdir)
                                            .font(FontId::monospace(10.0))
                                            .color(TEXT_MED),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    if !date_str.is_empty() {
                                        ui.label(
                                            RichText::new(&date_str)
                                                .font(FontId::monospace(10.0))
                                                .color(TEXT_MED),
                                        );
                                        ui.label(
                                            RichText::new("•")
                                                .font(FontId::monospace(10.0))
                                                .color(GRAY_BORDER),
                                        );
                                    }
                                    let matches_count = if self.lang == AppLanguage::Finnish {
                                        format!("{} osumaa", result.matches.len())
                                    } else {
                                        format!("{} matches", result.matches.len())
                                    };
                                    ui.label(
                                        RichText::new(matches_count)
                                            .font(FontId::monospace(10.0))
                                            .color(TEXT_MED),
                                    );
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    let open_btn_label = if self.lang == AppLanguage::Finnish {
                                        "Avaa"
                                    } else {
                                        "Open"
                                    };
                                    let folder_btn_label = if self.lang == AppLanguage::Finnish {
                                        "Kansio"
                                    } else {
                                        "Folder"
                                    };
                                    let copy_path_label = if self.lang == AppLanguage::Finnish {
                                        "Kopioi polku"
                                    } else {
                                        "Copy Path"
                                    };
                                    if ui
                                        .small_button(RichText::new(open_btn_label).color(BLUE_MED))
                                        .clicked()
                                    {
                                        self.open_file_async(&result.file);
                                    }
                                    let fm_tooltip = if self.lang == AppLanguage::Finnish {
                                        format!("Näytä: {}", os_file_manager_name(self.lang))
                                    } else {
                                        format!("Show in {}", os_file_manager_name(self.lang))
                                    };
                                    if ui
                                        .small_button(
                                            RichText::new(folder_btn_label).color(TEXT_MED),
                                        )
                                        .on_hover_text(fm_tooltip)
                                        .clicked()
                                    {
                                        self.open_in_file_manager_async(&result.file);
                                    }
                                    if ui
                                        .small_button(
                                            RichText::new(copy_path_label).color(TEXT_MED),
                                        )
                                        .clicked()
                                    {
                                        ui.output_mut(|o| {
                                            o.copied_text =
                                                result.file.to_string_lossy().to_string()
                                        });
                                    }
                                });

                                ui.add_space(2.0);
                                ui.separator();
                            }
                        });
                });

            self.selected_result = new_sel;
        }

        // Right panel: matches
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.results.is_empty() {
                ui.centered_and_justified(|ui| {
                    let prompt_text: String = if self.state == SearchState::Searching {
                        if self.lang == AppLanguage::Finnish { "Haetaan...".to_string() } else { "Searching...".to_string() }
                    } else if self.state == SearchState::Done {
                        if self.lang == AppLanguage::Finnish {
                            format!("Ei osumia hakusanalle \"{}\"", self.opts.query)
                        } else {
                            format!("No matches found for \"{}\"", self.opts.query)
                        }
                    } else {
                        if self.lang == AppLanguage::Finnish { "Kirjoita hakusana ja paina Hae".to_string() } else { "Enter a search term and press Search".to_string() }
                    };
                    ui.label(RichText::new(prompt_text)
                        .font(FontId::proportional(18.0)).color(if self.state == SearchState::Done { TEXT_MED } else { GRAY_BORDER }));
                });
                return;
            }

            let Some(sel) = self.selected_result else {
                ui.centered_and_justified(|ui| {
                    let select_text = if self.lang == AppLanguage::Finnish {
                        "Valitse tiedosto listasta"
                    } else {
                        "Select a file from the list"
                    };
                    ui.label(RichText::new(select_text)
                        .font(FontId::proportional(16.0)).color(GRAY_BORDER));
                });
                return;
            };

            let Some(result) = self.results.get(sel) else { return; };
            let fname = result.file.file_name()
                .unwrap_or_default().to_string_lossy().to_string();

            // Header
            ui.horizontal(|ui| {
                ui.heading(RichText::new(&fname).color(TEXT_DARK));
                ui.add_space(8.0);
                let header_matches = if self.lang == AppLanguage::Finnish {
                    format!("— {} osumaa", result.matches.len())
                } else {
                    format!("— {} matches", result.matches.len())
                };
                ui.label(RichText::new(header_matches).color(TEXT_MED));
                ui.add_space(16.0);
                let open_file_text = if self.lang == AppLanguage::Finnish { "Avaa tiedosto" } else { "Open File" };
                if ui.button(
                    RichText::new(open_file_text).color(Color32::WHITE)
                ).clicked() {
                    self.open_file_async(&result.file);
                }
                let show_fm_text = if self.lang == AppLanguage::Finnish {
                    format!("Näytä: {}", os_file_manager_name(self.lang))
                } else {
                    format!("Show in {}", os_file_manager_name(self.lang))
                };
                if ui.button(
                    RichText::new(show_fm_text).color(TEXT_DARK)
                ).clicked() {
                    self.open_in_file_manager_async(&result.file);
                }
                let copy_path_text = if self.lang == AppLanguage::Finnish { "Kopioi polku" } else { "Copy Path" };
                if ui.button(
                    RichText::new(copy_path_text).color(TEXT_DARK)
                ).clicked() {
                    ui.output_mut(|o| o.copied_text = result.file.to_string_lossy().to_string());
                }
            });
            ui.label(RichText::new(result.file.to_string_lossy().as_ref())
                .font(FontId::monospace(11.0)).color(TEXT_MED));
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            if result.matches.len() >= search::MAX_MATCHES_PER_FILE {
                let limit_notice = if self.lang == AppLanguage::Finnish {
                    format!("⚠️ Näytetään ensimmäiset {} osumaa (rajattu suorituskyvyn varmistamiseksi)", search::MAX_MATCHES_PER_FILE)
                } else {
                    format!("⚠️ Showing first {} matches (limited for performance)", search::MAX_MATCHES_PER_FILE)
                };
                ui.label(RichText::new(limit_notice).font(FontId::proportional(11.0)).color(ORANGE));
                ui.add_space(4.0);
            }

            // Match list (dynamic height scroll area for variable-length snippets)
            egui::ScrollArea::vertical()
                .id_source("match_list")
                .show(ui, |ui| {
                    for (mi, m) in result.matches.iter().enumerate() {
                        // Match header with copy button
                        ui.horizontal(|ui| {
                            let match_header = if self.lang == AppLanguage::Finnish {
                                format!("Osuma #{}", mi + 1)
                            } else {
                                format!("Match #{}", mi + 1)
                            };
                            let copy_btn_text = if self.lang == AppLanguage::Finnish { "Kopioi" } else { "Copy" };
                            ui.label(RichText::new(match_header)
                                .font(FontId::monospace(11.0))
                                .color(TEXT_MED));
                            if ui.small_button(
                                RichText::new(copy_btn_text).font(FontId::proportional(10.0)).color(TEXT_MED)
                            ).clicked() {
                                ui.output_mut(|o| o.copied_text = m.context.clone());
                            }
                        });
                        ui.add_space(2.0);
                        // Context wraps to full width
                        render_highlighted(ui, &m.context,
                            &self.opts.query, self.opts.ignore_case);
                        ui.add_space(6.0);
                        ui.separator();
                        ui.add_space(6.0);
                    }
                });
        });
    }
}

pub fn file_type_color(file_type: &str) -> Color32 {
    match file_type {
        "DOCX" => BLUE_MED,
        "ODT" | "FODT" => GREEN,
        "PDF" => ORANGE,
        "TXT" => PURPLE,
        _ => TEXT_MED,
    }
}

fn ftbtn(ui: &mut egui::Ui, enabled: &mut bool, label: &str, color: Color32) {
    let (fill, stroke, tc, icon) = if *enabled {
        (color, egui::Stroke::new(1.5, color), Color32::WHITE, "✓ ")
    } else {
        (
            Color32::from_rgb(241, 245, 249),
            egui::Stroke::new(1.0, GRAY_BORDER),
            TEXT_MED,
            "  ",
        )
    };

    let btn_text = format!("{}{}", icon, label);
    if ui
        .add(
            egui::Button::new(
                RichText::new(btn_text)
                    .font(FontId::monospace(11.0))
                    .color(tc)
                    .strong(),
            )
            .fill(fill)
            .stroke(stroke)
            .rounding(4.0)
            .min_size(Vec2::new(56.0, 24.0)),
        )
        .clicked()
    {
        *enabled = !*enabled;
    }
}

fn render_highlighted(ui: &mut egui::Ui, context: &str, query: &str, ignore_case: bool) {
    if query.is_empty() {
        ui.label(
            RichText::new(context)
                .font(FontId::proportional(13.0))
                .color(TEXT_DARK),
        );
        return;
    }

    let spans = search::find_match_spans(context, query, ignore_case);
    if spans.is_empty() {
        ui.label(
            RichText::new(context)
                .font(FontId::proportional(13.0))
                .color(TEXT_DARK),
        );
        return;
    }

    let mut job = egui::text::LayoutJob::default();
    let normal = egui::TextFormat {
        font_id: FontId::proportional(13.0),
        color: TEXT_DARK,
        ..Default::default()
    };
    let hi = egui::TextFormat {
        font_id: FontId::proportional(13.0),
        color: Color32::WHITE,
        background: RED_ACCENT,
        ..Default::default()
    };

    let mut last = 0;
    for (start, end) in spans {
        let safe_start = start.clamp(last, context.len());
        let safe_end = end.clamp(safe_start, context.len());

        if safe_start > last
            && context.is_char_boundary(last)
            && context.is_char_boundary(safe_start)
        {
            job.append(&context[last..safe_start], 0.0, normal.clone());
        }
        if safe_end > safe_start
            && context.is_char_boundary(safe_start)
            && context.is_char_boundary(safe_end)
        {
            job.append(&context[safe_start..safe_end], 0.0, hi.clone());
            last = safe_end;
        }
    }
    if last < context.len() && context.is_char_boundary(last) {
        job.append(&context[last..], 0.0, normal);
    }

    job.wrap.max_width = ui.available_width();
    ui.add(egui::Label::new(job).wrap(true));
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DoXsearch")
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "DoXsearch",
        options,
        Box::new(|cc| Box::new(DoXsearchApp::new(cc)) as Box<dyn eframe::App>),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_path_ascii() {
        let path = "/home/user/documents/projects/app";
        let truncated = truncate_path(path, 20);
        assert_eq!(truncated.chars().count(), 20);
        assert!(truncated.starts_with("..."));
    }

    #[test]
    fn test_truncate_path_multibyte_no_panic() {
        let path = "/home/käyttäjä/töitä_ja_asiakirjoja/projekti";
        let truncated = truncate_path(path, 26);
        assert_eq!(truncated.chars().count(), 26);
        assert!(truncated.starts_with("..."));
    }

    #[test]
    fn test_truncate_path_short() {
        let path = "/home/doc";
        let truncated = truncate_path(path, 26);
        assert_eq!(truncated, path);
    }

    #[test]
    fn test_resolve_directory_path_tilde() {
        let resolved = resolve_directory_path("~/Documents");
        assert!(!resolved.to_string_lossy().starts_with('~'));
        assert!(resolved.to_string_lossy().ends_with("Documents"));
    }

    #[test]
    fn test_resolve_directory_path_quoted() {
        let resolved = resolve_directory_path("'/tmp/test'");
        assert_eq!(resolved, PathBuf::from("/tmp/test"));
    }

    #[test]
    fn test_clean_path_windows_prefix() {
        let p = PathBuf::from(r"\\?\C:\Users\test\docs");
        let cleaned = clean_path(p);
        assert_eq!(cleaned, PathBuf::from(r"C:\Users\test\docs"));

        let normal = PathBuf::from("/home/user/docs");
        assert_eq!(clean_path(normal.clone()), normal);
    }

    #[test]
    fn test_save_results_csv_escapes_fields_and_writes_bom() {
        let path =
            std::env::temp_dir().join(format!("doxsearch-test-{}-results.csv", std::process::id()));
        let results = vec![SearchResult {
            file: PathBuf::from("/tmp/a,\"b.pdf"),
            file_type: "PDF".to_string(),
            matches: vec![search::Match {
                context: "first line, \"match\" with ääkköset".to_string(),
            }],
            modified: None,
        }];

        save_results_csv(&path, "term", &results).expect("CSV save failed");
        let bytes = std::fs::read(&path).expect("CSV read failed");
        std::fs::remove_file(&path).expect("CSV cleanup failed");

        // Verify UTF-8 BOM is present
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        let csv = String::from_utf8(bytes[3..].to_vec()).expect("Valid UTF-8 after BOM");
        assert!(csv.contains("\"/tmp/a,\"\"b.pdf\""));
        assert!(csv.contains("\"first line, \"\"match\"\" with ääkköset\""));
    }

    #[test]
    fn test_os_name_and_file_manager_label() {
        let os = os_name();
        assert!(!os.is_empty());
        let fm_fi = os_file_manager_name(AppLanguage::Finnish);
        assert!(!fm_fi.is_empty());
        let fm_en = os_file_manager_name(AppLanguage::English);
        assert!(!fm_en.is_empty());
    }

    #[test]
    fn test_language_labels_and_detection() {
        assert_eq!(AppLanguage::Finnish.label(), "Suomi");
        assert_eq!(AppLanguage::English.label(), "English");
        let detected = AppLanguage::detect_from_system();
        assert!(detected == AppLanguage::Finnish || detected == AppLanguage::English);

        // Check enum labels in both languages
        assert_eq!(
            SortOrder::DateDesc.label(AppLanguage::Finnish),
            "Päivämäärä (Uusin ensin)"
        );
        assert_eq!(
            SortOrder::DateDesc.label(AppLanguage::English),
            "Date (Newest first)"
        );
        assert_eq!(DateFilter::All.label(AppLanguage::Finnish), "Kaikki ajat");
        assert_eq!(DateFilter::All.label(AppLanguage::English), "All time");
        assert_eq!(
            SizeFilter::NoLimit.label(AppLanguage::Finnish),
            "Ei kokorajoitusta"
        );
        assert_eq!(
            SizeFilter::NoLimit.label(AppLanguage::English),
            "No size limit"
        );
    }

    #[test]
    fn test_linux_file_manager_detection() {
        let fm = detect_linux_file_manager();
        // Check that detect_linux_file_manager returns a valid variant
        let label = os_file_manager_name(AppLanguage::Finnish);
        assert!(!label.is_empty());
        let _ = fm;
    }

    #[test]
    fn test_user_directories_not_empty() {
        let home = get_user_home_dir();
        assert!(!home.as_os_str().is_empty());
        let docs = get_user_documents_dir();
        assert!(!docs.as_os_str().is_empty());
        let dl = get_user_downloads_dir();
        assert!(!dl.as_os_str().is_empty());
    }

    #[test]
    fn test_path_to_file_uri_encoding() {
        let p1 = Path::new("/home/user/document.pdf");
        assert_eq!(path_to_file_uri(p1), "file:///home/user/document.pdf");

        let p2 = Path::new("/home/user/My Documents/report #1 (2025) 100%.pdf");
        assert_eq!(
            path_to_file_uri(p2),
            "file:///home/user/My%20Documents/report%20%231%20%282025%29%20100%25.pdf"
        );

        let p3 = Path::new("/home/user/tiedosto_ääkköset.pdf");
        // UTF-8 bytes for 'ä' is 0xC3 0xA4, 'ö' is 0xC3 0xB6
        assert_eq!(
            path_to_file_uri(p3),
            "file:///home/user/tiedosto_%C3%A4%C3%A4kk%C3%B6set.pdf"
        );
    }

    #[test]
    fn test_recent_directories_mru_order() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = DoXsearchApp {
            opts: SearchOptions::default(),
            directory_input: "/dir1".to_string(),
            state: SearchState::Idle,
            results: Vec::new(),
            errors: Vec::new(),
            error: None,
            status_info: None,
            progress_count: (0, 0),
            selected_result: None,
            sort_order: SortOrder::DateDesc,
            date_filter: DateFilter::All,
            size_filter: SizeFilter::NoLimit,
            recent_directories: vec![
                "/dir1".to_string(),
                "/dir2".to_string(),
                "/dir3".to_string(),
            ],
            cache: DocumentCache::new(),
            last_search_stats: None,
            lang: AppLanguage::Finnish,
            current_search_id: 0,
            cancel_flag: None,
            tx,
            rx,
        };

        // Select /dir3 and simulate MRU insertion
        app.directory_input = "/dir3".to_string();
        let dir_str = app.directory_input.clone();
        if let Some(pos) = app.recent_directories.iter().position(|d| d == &dir_str) {
            app.recent_directories.remove(pos);
        }
        app.recent_directories.insert(0, dir_str);

        assert_eq!(app.recent_directories[0], "/dir3");
        assert_eq!(app.recent_directories[1], "/dir1");
        assert_eq!(app.recent_directories[2], "/dir2");
    }

    #[test]
    fn test_cancel_search_increments_id() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut app = DoXsearchApp {
            opts: SearchOptions::default(),
            directory_input: ".".to_string(),
            state: SearchState::Searching,
            results: Vec::new(),
            errors: Vec::new(),
            error: None,
            status_info: None,
            progress_count: (5, 10),
            selected_result: None,
            sort_order: SortOrder::DateDesc,
            date_filter: DateFilter::All,
            size_filter: SizeFilter::NoLimit,
            recent_directories: vec![],
            cache: DocumentCache::new(),
            last_search_stats: None,
            lang: AppLanguage::Finnish,
            current_search_id: 1,
            cancel_flag: Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                false,
            ))),
            tx,
            rx,
        };

        app.cancel_search();
        assert_eq!(app.current_search_id, 2);
        assert_eq!(app.state, SearchState::Idle);
        assert!(app.status_info.is_some());
    }

    #[test]
    fn test_show_in_file_manager_nonexistent_returns_err() {
        let non_existent = Path::new("/this/path/absolutely/does/not/exist/doxsearch_12345.xyz");
        let result = show_in_file_manager(non_existent);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("does not exist"));
    }

    #[test]
    fn test_file_type_color_matches_filter_buttons() {
        assert_eq!(file_type_color("DOCX"), BLUE_MED);
        assert_eq!(file_type_color("ODT"), GREEN);
        assert_eq!(file_type_color("FODT"), GREEN);
        assert_eq!(file_type_color("PDF"), ORANGE);
        assert_eq!(file_type_color("TXT"), PURPLE);
        assert_eq!(file_type_color("UNKNOWN"), TEXT_MED);
    }
}
