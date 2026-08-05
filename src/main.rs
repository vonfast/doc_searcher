mod search;

use crossbeam_channel::{bounded, Receiver, Sender};
use eframe::egui;
use egui::{Color32, FontId, RichText, Vec2};
use search::{SearchError, SearchOptions, SearchResult};
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
    Progress(String),
    Done {
        results: Vec<SearchResult>,
        errors: Vec<SearchError>,
    },
    Error(String),
}

#[derive(PartialEq)]
enum SearchState { Idle, Searching, Done }

struct DoXgrepApp {
    opts: SearchOptions,
    directory_input: String,
    state: SearchState,
    results: Vec<SearchResult>,
    errors: Vec<SearchError>,
    error: Option<String>,
    progress_msg: String,
    selected_result: Option<usize>,
    tx: Sender<SearchMessage>,
    rx: Receiver<SearchMessage>,
}

impl DoXgrepApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = bounded(256);
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        Self {
            directory_input: home.clone(),
            opts: SearchOptions { directory: PathBuf::from(&home), ..Default::default() },
            state: SearchState::Idle,
            results: Vec::new(),
            errors: Vec::new(),
            error: None,
            progress_msg: String::new(),
            selected_result: None,
            tx, rx,
        }
    }

    fn start_search(&mut self) {
        if self.opts.query.trim().is_empty() {
            self.error = Some("Please enter a search term first.".to_string());
            return;
        }
        self.opts.directory = PathBuf::from(&self.directory_input);
        if !self.opts.directory.exists() {
            self.error = Some(format!("Directory not found: {}", self.directory_input));
            return;
        }
        self.state = SearchState::Searching;
        self.results.clear();
        self.errors.clear();
        self.error = None;
        self.selected_result = None;
        self.progress_msg = "Starting search...".to_string();

        let opts = self.opts.clone();
        let tx = self.tx.clone();
        thread::spawn(move || {
            let tx_p = tx.clone();
            match search::search_directory(&opts, move |msg| {
                let _ = tx_p.send(SearchMessage::Progress(msg));
            }) {
                Ok((results, errors)) => {
                    let _ = tx.send(SearchMessage::Done { results, errors });
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
        self.progress_msg.clear();
        self.selected_result = None;
    }

    fn poll_messages(&mut self, ctx: &egui::Context) {
        let mut got_msg = false;
        while let Ok(msg) = self.rx.try_recv() {
            got_msg = true;
            match msg {
                SearchMessage::Progress(s) => self.progress_msg = s,
                SearchMessage::Done { results, errors } => {
                    self.results = results;
                    self.errors = errors;
                    self.state = SearchState::Done;
                    self.progress_msg.clear();
                    if !self.results.is_empty() {
                        self.selected_result = Some(0);
                    }
                }
                SearchMessage::Error(e) => {
                    self.error = Some(e);
                    self.state = SearchState::Idle;
                }
            }
        }
        if got_msg {
            ctx.request_repaint();
        }
    }

    fn total_matches(&self) -> usize {
        self.results.iter().map(|r| r.matches.len()).sum()
    }
}

impl eframe::App for DoXgrepApp {
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
                    ui.label(RichText::new("DoXgrep")
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
                            ui.add_space(6.0);

                            ui.label(RichText::new("File types:").color(TEXT_MED));
                            ui.add_space(4.0);
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
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label(RichText::new(&self.progress_msg)
                                        .font(FontId::proportional(11.0)).color(TEXT_MED).italics());
                                });
                            }

                            if let Some(err) = &self.error.clone() {
                                ui.add_space(6.0);
                                ui.colored_label(Color32::RED, err);
                            }

                            if self.state == SearchState::Done {
                                ui.add_space(12.0);
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
                .exact_width(260.0)
                .resizable(true)
                .show(ctx, |ui| {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.add_space(8.0);
                        ui.label(RichText::new(
                            format!("Files ({})", self.results.len()))
                            .font(FontId::proportional(12.0)).color(TEXT_MED).strong());
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
                                    .map(|p| {
                                        let s = p.to_string_lossy().to_string();
                                        if s.len() > 26 {
                                            format!("...{}", &s[s.len().saturating_sub(23)..])
                                        } else { s }
                                    })
                                    .unwrap_or_default();
                                let type_color = match result.file_type.as_str() {
                                    "DOCX" => BLUE_MED,
                                    "ODT"  => GREEN,
                                    "PDF"  => ORANGE,
                                    _      => TEXT_MED,
                                };

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

                                // Directory path + open button
                                ui.horizontal(|ui| {
                                    ui.add_space(4.0);
                                    ui.label(RichText::new(&fdir)
                                        .font(FontId::monospace(10.0)).color(TEXT_MED));
                                    ui.label(RichText::new(
                                        format!("({} matches)", result.matches.len()))
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
                let _ = open::that(&path);
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
                    let _ = open::that(&result.file);
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
        ui.label(RichText::new(context).font(FontId::proportional(13.0)).color(Color32::from_rgb(180, 180, 180)));
        return;
    }
    let ctx_cmp = if ignore_case { context.to_lowercase() } else { context.to_string() };
    let q_cmp   = if ignore_case { query.to_lowercase()   } else { query.to_string() };

    let mut job = egui::text::LayoutJob::default();
    let normal = egui::TextFormat {
        font_id: FontId::proportional(13.0), color: Color32::from_rgb(180, 180, 180), ..Default::default()
    };
    let hi = egui::TextFormat {
        font_id: FontId::proportional(13.0), color: Color32::WHITE,
        background: RED_ACCENT, ..Default::default()
    };

    let mut last = 0;
    while let Some(pos) = ctx_cmp[last..].find(&q_cmp) {
        let abs = last + pos;
        if abs > last { job.append(&context[last..abs], 0.0, normal.clone()); }
        
        // Lasketaan osuman pituus alkuperäisessä tekstissä.
        // Tämä on tarpeen, koska ignore_case-tilassa to_lowercase() voi muuttaa tavujen määrää.
        let mut end = abs;
        let mut match_len_in_lowercase = 0;
        let mut temp_it = context[abs..].chars();
        while match_len_in_lowercase < q_cmp.len() {
            if let Some(c) = temp_it.next() {
                let lowered = if ignore_case { c.to_lowercase().to_string() } else { c.to_string() };
                match_len_in_lowercase += lowered.len();
                end += c.len_utf8();
            } else {
                break;
            }
        }

        job.append(&context[abs..end], 0.0, hi.clone());
        last = end;
        if last >= ctx_cmp.len() { break; }
    }
    if last < context.len() { job.append(&context[last..], 0.0, normal); }
    // Ensure text wraps and is not clipped
    job.wrap.max_width = f32::INFINITY;
    ui.add(egui::Label::new(job).wrap(true));
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DoXgrep")
            .with_inner_size([1200.0, 750.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native("DoXgrep", options,
        Box::new(|cc| Box::new(DoXgrepApp::new(cc)) as Box<dyn eframe::App>))
}
