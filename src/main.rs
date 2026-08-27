mod search;

use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use egui::{Color32, FontId, RichText, Vec2};
use search::{SearchError, SearchOptions, SearchResult};
use std::io::Write;
use std::path::PathBuf;
use std::thread;

const BLUE_DARK:  Color32 = Color32::from_rgb(30,  58,  138);
const BLUE_MED:   Color32 = Color32::from_rgb(59,  130, 246);
const GREEN:      Color32 = Color32::from_rgb(34,  197, 94);
const ORANGE:     Color32 = Color32::from_rgb(249, 115, 22);
const RED_ACCENT: Color32 = Color32::from_rgb(239, 68,  68);
const GRAY_BORDER:Color32 = Color32::from_rgb(203, 213, 225);
const TEXT_DARK:  Color32 = Color32::from_rgb(15,  23,  42);
const TEXT_MED:   Color32 = Color32::from_rgb(71,  85,  105);

enum SearchMessage {
    Progress { processed: usize, total: usize },
    MatchFound(SearchResult),
    ErrorFound(SearchError),
    Done,
    Error(String),
}

use chrono::{DateTime, Local};

#[derive(PartialEq, Clone, Copy, Debug)]
enum SortOrder {
    DateDesc,
    DateAsc,
    Name,
    Matches,
}

impl SortOrder {
    fn label(&self) -> &'static str {
        match self {
            SortOrder::DateDesc => "Date (Newest first)",
            SortOrder::DateAsc => "Date (Oldest first)",
            SortOrder::Name => "Name (A-Z)",
            SortOrder::Matches => "Match Count",
        }
    }
}

#[derive(PartialEq)]
enum SearchState { Idle, Searching, Done }

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

pub fn resolve_directory_path(input: &str) -> PathBuf {
    let trimmed = input.trim().trim_matches(|c| c == '\'' || c == '"');
    if trimmed.is_empty() {
        return PathBuf::from(".");
    }
    if trimmed == "~" {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        return PathBuf::from(home);
    }
    if let Some(stripped) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        return PathBuf::from(home).join(stripped);
    }
    PathBuf::from(trimmed)
}

struct DoXsearchApp {
    opts: SearchOptions,
    directory_input: String,
    state: SearchState,
    results: Vec<SearchResult>,
    errors: Vec<SearchError>,
    error: Option<String>,
    progress_count: (usize, usize),
    selected_result: Option<usize>,
    sort_order: SortOrder,
    tx: Sender<SearchMessage>,
    rx: Receiver<SearchMessage>,
}

impl DoXsearchApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::light());
        let (tx, rx) = bounded(256);
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        Self {
            directory_input: home.clone(),
            opts: SearchOptions { directory: PathBuf::from(&home), ..Default::default() },
            state: SearchState::Idle,
            results: Vec::new(),
            errors: Vec::new(),
            error: None,
            progress_count: (0, 0),
            selected_result: None,
            sort_order: SortOrder::DateDesc,
            tx,
            rx,
        }
    }

    fn sort_results(&mut self) {
        let selected_path = self.selected_result.and_then(|idx| self.results.get(idx).map(|r| r.file.clone()));

        match self.sort_order {
            SortOrder::DateDesc => {
                self.results.sort_by(|a, b| b.modified.cmp(&a.modified));
            }
            SortOrder::DateAsc => {
                self.results.sort_by(|a, b| a.modified.cmp(&b.modified));
            }
            SortOrder::Name => {
                self.results.sort_by(|a, b| {
                    let name_a = a.file.file_name().map(|n| n.to_string_lossy().to_lowercase());
                    let name_b = b.file.file_name().map(|n| n.to_string_lossy().to_lowercase());
                    name_a.cmp(&name_b)
                });
            }
            SortOrder::Matches => {
                self.results.sort_by(|a, b| b.matches.len().cmp(&a.matches.len()));
            }
        }

        if let Some(path) = selected_path {
            self.selected_result = self.results.iter().position(|r| r.file == path);
        }
    }

    fn start_search(&mut self) {
        if self.opts.query.trim().is_empty() {
            self.error = Some("Please enter a search term first.".to_string());
            return;
        }
        if !self.opts.search_docx && !self.opts.search_odt && !self.opts.search_pdf {
            self.error = Some("Please select at least one file type (DOCX, ODT, PDF).".to_string());
            return;
        }

        self.opts.directory = resolve_directory_path(&self.directory_input);
        if !self.opts.directory.exists() {
            self.error = Some(format!("Directory not found: {}", self.directory_input));
            return;
        }
        self.state = SearchState::Searching;
        self.results.clear();
        self.errors.clear();
        self.error = None;
        self.selected_result = None;
        self.progress_count = (0, 0);

        let opts = self.opts.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let tx_match = tx.clone();
            let tx_err = tx.clone();
            let tx_prog = tx.clone();
            match search::search_directory(
                &opts,
                move |result| {
                    let _ = tx_match.send(SearchMessage::MatchFound(result));
                },
                move |error| {
                    let _ = tx_err.send(SearchMessage::ErrorFound(error));
                },
                move |processed, total| {
                    let _ = tx_prog.try_send(SearchMessage::Progress { processed, total });
                },
            ) {
                Ok(()) => {
                    let _ = tx.send(SearchMessage::Done);
                }
                Err(e) => {
                    let _ = tx.send(SearchMessage::Error(e.to_string()));
                }
            }
        });
    }

    fn clear_search(&mut self) {
        self.results.clear();
        self.errors.clear();
        self.opts.query.clear();
        self.state = SearchState::Idle;
        self.error = None;
        self.progress_count = (0, 0);
        self.selected_result = None;
    }

    fn poll_messages(&mut self, ctx: &egui::Context) {
        let mut got_msg = false;
        let mut new_matches = false;
        while let Ok(msg) = self.rx.try_recv() {
            got_msg = true;
            match msg {
                SearchMessage::Progress { processed, total } => {
                    self.progress_count = (processed, total);
                }
                SearchMessage::MatchFound(result) => {
                    self.results.push(result);
                    new_matches = true;
                    if self.selected_result.is_none() {
                        self.selected_result = Some(0);
                    }
                }
                SearchMessage::ErrorFound(err) => {
                    self.errors.push(err);
                }
                SearchMessage::Done => {
                    self.sort_results();
                    self.state = SearchState::Done;
                    self.progress_count = (0, 0);
                    if !self.results.is_empty() && self.selected_result.is_none() {
                        self.selected_result = Some(0);
                    }
                }
                SearchMessage::Error(e) => {
                    self.error = Some(e);
                    self.state = SearchState::Idle;
                }
            }
        }
        if new_matches {
            self.sort_results();
        }
        if got_msg {
            ctx.request_repaint();
        }
    }

    fn total_matches(&self) -> usize {
        self.results.iter().map(|r| r.matches.len()).sum()
    }

    fn save_results(&mut self) {
        if self.results.is_empty() {
            self.error = Some("There are no results to save.".to_string());
            return;
        }

        let Some(path) = rfd::FileDialog::new()
            .set_file_name("doxsearch-results.csv")
            .add_filter("CSV files", &["csv"])
            .save_file()
        else {
            return;
        };

        match save_results_csv(&path, &self.opts.query, &self.results) {
            Ok(()) => self.error = Some(format!("Results saved to {}", path.display())),
            Err(e) => self.error = Some(format!("Failed to save results: {e}")),
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
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "query,file,file_type,match_number,context")?;
    for result in results {
        for (index, found_match) in result.matches.iter().enumerate() {
            writeln!(
                file,
                "{},{},{},{},{}",
                csv_field(query),
                csv_field(&result.file.to_string_lossy()),
                csv_field(&result.file_type),
                index + 1,
                csv_field(&found_match.context)
            )?;
        }
    }
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
                    ui.label(RichText::new("DoXsearch")
                        .font(FontId::proportional(22.0))
                        .color(BLUE_DARK).strong());
                    ui.add_space(12.0);
                    ui.label(RichText::new("| Search text in DOCX, ODT and PDF files")
                        .font(FontId::proportional(13.0))
                        .color(TEXT_MED));
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
                            ui.label(RichText::new("Directory").color(TEXT_MED).strong());
                            ui.add_space(3.0);
                            ui.add(egui::TextEdit::singleline(&mut self.directory_input)
                                .desired_width(240.0)
                                .hint_text("/home/user/docs")
                                .font(FontId::monospace(11.0)));
                            ui.add_space(3.0);
                            if ui.button("Browse...").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .pick_folder()
                                {
                                    self.directory_input = path.to_string_lossy().to_string();
                                    self.error = None;
                                }
                            }
                            ui.add_space(10.0);
                            ui.separator();
                            ui.add_space(10.0);

                            ui.label(RichText::new("Search term").color(TEXT_MED).strong());
                            ui.add_space(3.0);
                            let qr = ui.add(egui::TextEdit::singleline(&mut self.opts.query)
                                .desired_width(240.0)
                                .hint_text("Type search term...")
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

                            ui.label(RichText::new("Settings").color(TEXT_MED).strong());
                            ui.add_space(4.0);
                            ui.checkbox(&mut self.opts.ignore_case, "Ignore case");
                            ui.checkbox(&mut self.opts.recursive, "Recursive search");
                            ui.checkbox(&mut self.opts.search_hidden, "Search hidden folders");
                            ui.horizontal(|ui| {
                                ftbtn(ui, &mut self.opts.search_docx, "DOCX", BLUE_MED);
                                ftbtn(ui, &mut self.opts.search_odt,  "ODT",  GREEN);
                                ftbtn(ui, &mut self.opts.search_pdf,  "PDF",  ORANGE);
                            });
                            ui.add_space(6.0);

                            ui.label(RichText::new("Context:").color(TEXT_MED));
                            ui.add(egui::Slider::new(&mut self.opts.context_size, 50..=500).text(""));
                            ui.add_space(12.0);

                            let searching = self.state == SearchState::Searching;
                            ui.horizontal(|ui| {
                                if ui.add_enabled(
                                    !searching,
                                    egui::Button::new(
                                        RichText::new(if searching { "Searching..." } else { "Search" })
                                            .font(FontId::proportional(15.0)).strong()
                                    ).min_size(Vec2::new(115.0, 36.0))
                                ).clicked() {
                                    self.start_search();
                                }

                                if ui.add_enabled(
                                    !searching,
                                    egui::Button::new(
                                        RichText::new("Clear")
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
                                } else {
                                    "Scanning directory...".to_string()
                                };
                                ui.add(egui::ProgressBar::new(fraction).text(text).animate(true));
                            }

                            if let Some(err) = &self.error.clone() {
                                ui.add_space(6.0);
                                ui.colored_label(Color32::RED, err);
                            }

                            if self.state == SearchState::Done {
                                ui.add_space(12.0);
                                if ui.button("Save results...").clicked() {
                                    self.save_results();
                                }
                                let total = self.total_matches();
                                if total > 0 {
                                    ui.label(RichText::new(
                                        format!("{} matches in {} files", total, self.results.len()))
                                        .color(Color32::DARK_GREEN).strong());
                                } else {
                                    ui.label(RichText::new("No matches found").color(TEXT_MED));
                                }

                                if !self.errors.is_empty() {
                                    ui.add_space(8.0);
                                    ui.separator();
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(format!("Errors ({})", self.errors.len()))
                                        .color(RED_ACCENT).strong());
                                    
                                    for err in &self.errors {
                                        let fname = err.file.file_name()
                                            .unwrap_or_default().to_string_lossy();
                                        ui.label(RichText::new(format!("• {}: {}", fname, err.error))
                                            .font(FontId::proportional(11.0))
                                            .color(RED_ACCENT));
                                    }
                                }
                            }
                        });
                    });
                });
            });

        // File list panel (center)
        if !self.results.is_empty() {
            let mut new_sel = self.selected_result;
            let mut open_path: Option<PathBuf> = None;

            egui::SidePanel::left("file_list_panel")
                .exact_width(280.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(4.0);
                        ui.label(RichText::new(
                            format!("Files ({})", self.results.len()))
                            .font(FontId::proportional(12.0)).color(TEXT_MED).strong());

                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let prev_sort = self.sort_order;
                            egui::ComboBox::from_id_source("sort_order_combo")
                                .selected_text(RichText::new(self.sort_order.label()).font(FontId::proportional(11.0)))
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut self.sort_order, SortOrder::DateDesc, SortOrder::DateDesc.label());
                                    ui.selectable_value(&mut self.sort_order, SortOrder::DateAsc, SortOrder::DateAsc.label());
                                    ui.selectable_value(&mut self.sort_order, SortOrder::Name, SortOrder::Name.label());
                                    ui.selectable_value(&mut self.sort_order, SortOrder::Matches, SortOrder::Matches.label());
                                });
                            if prev_sort != self.sort_order {
                                self.sort_results();
                            }
                        });
                    });
                    ui.add_space(4.0);
                    ui.separator();

                    egui::ScrollArea::vertical()
                        .id_source("file_list")
                        .show(ui, |ui| {
                            for (ri, result) in self.results.iter().enumerate() {
                                let is_sel = new_sel == Some(ri);
                                let fname = result.file.file_name()
                                    .unwrap_or_default().to_string_lossy().to_string();
                                let fdir = result.file.parent()
                                    .map(|p| truncate_path(&p.to_string_lossy(), 26))
                                    .unwrap_or_default();
                                let type_color = match result.file_type.as_str() {
                                    "DOCX" => BLUE_MED,
                                    "ODT"  => GREEN,
                                    "PDF"  => ORANGE,
                                    _      => TEXT_MED,
                                };

                                let date_str = result.modified
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
                                        .strong()
                                );
                                if ui.add(sel_label).clicked() {
                                    new_sel = Some(ri);
                                }
                                ui.add_space(2.0);

                                // Directory path + Date + Matches + open button
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(&fdir)
                                        .font(FontId::monospace(10.0)).color(TEXT_MED));
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    if !date_str.is_empty() {
                                        ui.label(RichText::new(&date_str)
                                            .font(FontId::monospace(10.0)).color(TEXT_MED));
                                        ui.label(RichText::new("•")
                                            .font(FontId::monospace(10.0)).color(TEXT_MED));
                                    }
                                    ui.label(RichText::new(
                                        format!("{} matches", result.matches.len()))
                                        .font(FontId::proportional(10.0)).color(TEXT_MED));
                                });
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    if ui.small_button(
                                        RichText::new("Open File").color(BLUE_MED)
                                    ).clicked() {
                                        open_path = Some(result.file.clone());
                                    }
                                });

                                ui.add_space(2.0);
                                ui.separator();
                            }
                        });
                });

            self.selected_result = new_sel;
            if let Some(path) = open_path {
                if let Err(e) = open::that(&path) {
                    self.error = Some(format!("Failed to open file: {}", e));
                }
            }
        }

        // Right panel: matches
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.results.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(
                        if self.state == SearchState::Searching {
                            "Searching..."
                        } else {
                            "Enter a search term and press Search"
                        }
                    ).font(FontId::proportional(18.0)).color(GRAY_BORDER));
                });
                return;
            }

            let Some(sel) = self.selected_result else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("Select a file from the list")
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
                ui.label(RichText::new(
                    format!("— {} matches", result.matches.len()))
                    .color(TEXT_MED));
                ui.add_space(16.0);
                if ui.button(
                    RichText::new("Open File").color(Color32::WHITE)
                ).clicked() {
                    if let Err(e) = open::that(&result.file) {
                        self.error = Some(format!("Failed to open file: {}", e));
                    }
                }
            });
            ui.label(RichText::new(result.file.to_string_lossy().as_ref())
                .font(FontId::monospace(11.0)).color(TEXT_MED));
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);

            // Match list
            egui::ScrollArea::vertical()
                .id_source("match_list")
                .show(ui, |ui| {
                    for (mi, m) in result.matches.iter().enumerate() {
                        // Match number on its own line
                        ui.label(RichText::new(format!("Match #{}", mi + 1))
                            .font(FontId::monospace(11.0))
                            .color(TEXT_MED));
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

fn ftbtn(ui: &mut egui::Ui, enabled: &mut bool, label: &str, color: Color32) {
    let fill = if *enabled { color } else { GRAY_BORDER };
    let tc   = if *enabled { Color32::WHITE } else { TEXT_MED };
    if ui.add(egui::Button::new(
        RichText::new(label).font(FontId::monospace(11.0)).color(tc).strong()
    ).fill(fill).rounding(4.0).min_size(Vec2::new(48.0, 24.0))).clicked() {
        *enabled = !*enabled;
    }
}

fn render_highlighted(ui: &mut egui::Ui, context: &str, query: &str, ignore_case: bool) {
    if query.is_empty() {
        ui.label(RichText::new(context).font(FontId::proportional(13.0)).color(TEXT_DARK));
        return;
    }

    let spans = search::find_match_spans(context, query, ignore_case);
    if spans.is_empty() {
        ui.label(RichText::new(context).font(FontId::proportional(13.0)).color(TEXT_DARK));
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
        if start > last && start <= context.len() {
            job.append(&context[last..start], 0.0, normal.clone());
        }
        if end <= context.len() && end >= start {
            job.append(&context[start..end], 0.0, hi.clone());
            last = end;
        }
    }
    if last < context.len() {
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
    eframe::run_native("DoXsearch", options,
        Box::new(|cc| Box::new(DoXsearchApp::new(cc)) as Box<dyn eframe::App>))
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
    fn test_save_results_csv_escapes_fields() {
        let path = std::env::temp_dir().join(format!(
            "doxsearch-test-{}-results.csv",
            std::process::id()
        ));
        let results = vec![SearchResult {
            file: PathBuf::from("/tmp/a,\"b.pdf"),
            file_type: "PDF".to_string(),
            matches: vec![search::Match {
                context: "first line, \"match\"".to_string(),
            }],
            modified: None,
        }];

        save_results_csv(&path, "term", &results).expect("CSV save failed");
        let csv = std::fs::read_to_string(&path).expect("CSV read failed");
        std::fs::remove_file(&path).expect("CSV cleanup failed");

        assert!(csv.contains("\"/tmp/a,\"\"b.pdf\""));
        assert!(csv.contains("\"first line, \"\"match\"\"\""));
    }
}

