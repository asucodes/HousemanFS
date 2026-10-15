//! The desktop client: a native window over the same index the CLI builds.
//!
//! This crate renders. It holds no volume handles, opens no files for measurement, and
//! cannot initiate an action — the architecture requires that the interface decides
//! nothing, so that a rendering bug can never become a filesystem bug. Everything on
//! screen is a value it was given: index rows, shared accounting, committed rules.
//!
//! The look carries over from the finder: light blue, monospace figures, an instrument
//! rather than a product. A tool that reads a user's entire disk should feel plain,
//! inspectable, and obviously not trying to sell anything.
//!
//! Scanning runs on a worker thread ([`scan`]); every screen polls it once per frame and
//! renders whatever exists. Selection is a path, and the evidence panel explains it with
//! the same words the CLI's `explain` prints.

mod scan;
mod screens;
mod treemap;

use hfs_core::format::{human_age, human_bytes, human_bytes_exact, windows_ticks_now};
use hfs_core::is_volume_root;
use hfs_import::{VolumeAccounting, account_volume};
use hfs_index::{DirHit, FileHit, Index, ScanMetadata, Summary};
use hfs_model::{EvalReport, RuleConfig, eval_engine, run_eval};

use scan::ScanWorker;

/// The 1980s palette, carried over from the finder.
const FIELD: egui::Color32 = egui::Color32::from_rgb(0xB5, 0xD4, 0xF4);
const PANEL: egui::Color32 = egui::Color32::from_rgb(0xE6, 0xF1, 0xFB);
const INK: egui::Color32 = egui::Color32::from_rgb(0x04, 0x2C, 0x53);
const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x18, 0x5F, 0xA5);

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("HousemanFS")
            .with_inner_size([1200.0, 800.0]),
        ..Default::default()
    };
    eframe::run_native(
        "HousemanFS",
        options,
        Box::new(|context| Ok(Box::new(App::new(context)))),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Home,
    Dashboard,
    Browse,
    Map,
    Search,
    Suggest,
}

impl Screen {
    const ALL: [(Screen, &'static str); 6] = [
        (Screen::Home, "Scan"),
        (Screen::Dashboard, "Dashboard"),
        (Screen::Browse, "Browse"),
        (Screen::Map, "Map"),
        (Screen::Search, "Search"),
        (Screen::Suggest, "Suggest"),
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseTab {
    Files,
    Dirs,
    Extensions,
}

/// One suggestion ready to render: the file it concerns plus what the rule said.
#[derive(Clone)]
struct SuggestRow {
    path: String,
    allocated: u64,
    rule: String,
    version: u32,
    confidence: f64,
    verb: String,
    evidence: Vec<(String, String)>,
}

struct App {
    screen: Screen,
    scan_path: String,
    db_path: String,
    worker: Option<ScanWorker>,
    progress: (u64, u64),
    scan_error: Option<String>,
    notice: Option<String>,
    index: Option<Index>,
    scan_root: String,
    accounting: Option<VolumeAccounting>,
    summary: Summary,
    meta: ScanMetadata,
    files: Vec<FileHit>,
    dirs: Vec<DirHit>,
    exts: Vec<(String, u64, u64)>,
    selection: Option<String>,
    browse_tab: BrowseTab,
    find_term: String,
    find_rows: Vec<FileHit>,
    ask_size: usize,
    ask_ext: String,
    ask_older: usize,
    ask_shared: bool,
    ask_streams: bool,
    ask_kind: usize,
    ask_rows: Vec<FileHit>,
    threshold: f32,
    without: [bool; 5],
    min_bytes_text: String,
    suggest_rows: Vec<SuggestRow>,
    suggest_ran: bool,
    eval_report: EvalReport,
}

impl App {
    fn new(context: &eframe::CreationContext) -> Self {
        let mut visuals = egui::Visuals::light();
        visuals.window_fill = PANEL;
        visuals.panel_fill = PANEL;
        visuals.selection.bg_fill = ACCENT;
        visuals.selection.stroke = egui::Stroke::new(1.0_f32, PANEL);
        visuals.hyperlink_color = ACCENT;
        context.egui_ctx.set_visuals(visuals);

        // The baseline proves itself continuously: the score sits in the Suggest screen
        // header, recomputed every launch, so a regression cannot hide behind a stale
        // number.
        let eval_report = run_eval(&eval_engine(), &RuleConfig::default());
        Self {
            screen: Screen::Home,
            scan_path: String::new(),
            db_path: "housemanfs.db".to_string(),
            worker: None,
            progress: (0, 0),
            scan_error: None,
            notice: None,
            index: None,
            scan_root: String::new(),
            accounting: None,
            summary: Summary::default(),
            meta: ScanMetadata::default(),
            files: Vec::new(),
            dirs: Vec::new(),
            exts: Vec::new(),
            selection: None,
            browse_tab: BrowseTab::Files,
            find_term: String::new(),
            find_rows: Vec::new(),
            ask_size: 0,
            ask_ext: String::new(),
            ask_older: 0,
            ask_shared: false,
            ask_streams: false,
            ask_kind: 0,
            ask_rows: Vec::new(),
            threshold: 0.5,
            without: [false; 5],
            min_bytes_text: "1GB".to_string(),
            suggest_rows: Vec::new(),
            suggest_ran: false,
            eval_report,
        }
    }

    /// Open an index file and load everything the screens render. Failing leaves the
    /// previous state untouched: a corrupt file must not blank a working view.
    fn open_database(&mut self, db: &str) -> Result<(), String> {
        let index = Index::open(db).map_err(|e| e.to_string())?;
        let summary = index.summary().map_err(|e| e.to_string())?;
        let meta = index.scan_metadata();
        let files = index.largest_files(500).map_err(|e| e.to_string())?;
        let dirs = index.largest_dirs(500).map_err(|e| e.to_string())?;
        let exts = index.by_extension(200).map_err(|e| e.to_string())?;

        // A volume-rooted index reconciles live: the residual needs today's free space,
        // not the scan's. Anything else shows index totals without a verdict.
        let accounting = if is_volume_root(&meta.root) {
            let walk = hfs_win::WalkSummary {
                files: meta.files,
                directories: meta.directories,
                reparse_points: meta.reparse_points,
                allocated_bytes: summary.allocated,
                stream_bytes: summary.stream_bytes,
                directory_bytes: meta.directory_bytes,
                denied_directories: meta.denied_directories,
                denied_files: meta.denied_files,
                stream_unknown_files: meta.stream_unknown_files,
                ..Default::default()
            };
            account_volume(&meta.root, &walk).ok()
        } else {
            None
        };

        self.index = Some(index);
        self.summary = summary;
        self.meta = meta.clone();
        self.scan_root = meta.root;
        self.accounting = accounting;
        self.files = files;
        self.dirs = dirs;
        self.exts = exts;
        self.selection = None;
        self.find_rows.clear();
        self.ask_rows.clear();
        self.suggest_ran = false;
        Ok(())
    }

    /// Poll the worker once per frame. Progress updates counts; the single ending opens
    /// the index or reports the failure. Cancellation arrives as an error naming itself.
    fn poll_worker(&mut self, context: &egui::Context) {
        let finished = if let Some(worker) = self.worker.as_mut() {
            let (progress, done) = worker.poll();
            if let Some((files, dirs)) = progress {
                self.progress = (files, dirs);
            }
            if let Some(outcome) = done {
                match outcome {
                    Ok(done) => {
                        let db = done.db_path.clone();
                        match self.open_database(&db) {
                            Ok(()) => {
                                self.screen = Screen::Dashboard;
                                self.scan_error = None;
                                self.notice = done.refresh.map(|report| {
                                    format!(
                                        "refresh: +{} -{} ~{} moved {}",
                                        report.added_files,
                                        report.removed_files,
                                        report.modified_files,
                                        report.moved_files
                                    )
                                });
                            }
                            Err(error) => self.scan_error = Some(error),
                        }
                    }
                    Err(error) => self.scan_error = Some(error),
                }
                true
            } else {
                context.request_repaint();
                false
            }
        } else {
            false
        };
        if finished {
            self.worker = None;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_worker(context);

        egui::TopBottomPanel::top("nav").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("housemanfs").color(ACCENT).strong());
                ui.label(egui::RichText::new("read-only").color(ACCENT).strong());
                ui.separator();
                let open = self.index.is_some();
                for (screen, label) in Screen::ALL {
                    let enabled = open || screen == Screen::Home;
                    ui.add_enabled_ui(enabled, |ui| {
                        if ui.selectable_label(self.screen == screen, label).clicked() {
                            self.screen = screen;
                        }
                    });
                }
            });
        });

        if self.selection.is_some() {
            egui::SidePanel::right("evidence")
                .default_width(360.0)
                .show(context, |ui| {
                    self.show_explain(ui);
                });
        }

        egui::CentralPanel::default().show(context, |ui| match self.screen {
            Screen::Home => self.show_home(ui),
            Screen::Dashboard => self.show_dashboard(ui),
            Screen::Browse => self.show_browse(ui),
            Screen::Map => self.show_map(ui),
            Screen::Search => self.show_search(ui),
            Screen::Suggest => self.show_suggest(ui),
        });

        egui::TopBottomPanel::bottom("status").show(context, |ui| {
            ui.horizontal(|ui| {
                if self.worker.is_some() {
                    ui.label(format!(
                        "scanning: {} files, {} directories",
                        self.progress.0, self.progress.1
                    ));
                    if ui.button("Cancel").clicked() {
                        if let Some(worker) = &self.worker {
                            worker.cancel();
                        }
                    }
                } else if let Some(error) = &self.scan_error {
                    ui.colored_label(egui::Color32::DARK_RED, error);
                } else if let Some(notice) = &self.notice {
                    ui.label(notice);
                } else if self.index.is_some() {
                    ui.label(format!(
                        "{} indexed from {}",
                        self.db_label(),
                        self.scan_root
                    ));
                } else {
                    ui.label("no index open — pick a folder to begin");
                }
            });
        });
    }
}

/// Shared small widgets and actions, one place so the screens agree.
impl App {
    fn db_label(&self) -> String {
        format!(
            "{} files, {}",
            self.summary.files,
            human_bytes(self.summary.allocated)
        )
    }

    /// A horizontal usage bar: `filled` of `total`, with a label. Painted, not plotted —
    /// one rectangle is not worth a chart dependency.
    fn usage_bar(ui: &mut egui::Ui, filled: u64, total: u64, label: String) {
        let width = ui.available_width().max(120.0);
        let height = 22.0;
        let (rect, _) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 4.0, FIELD);
        if total > 0 {
            let fraction = (filled as f32 / total as f32).clamp(0.0, 1.0);
            let mut used = rect;
            used.max.x = rect.min.x + rect.width() * fraction;
            painter.rect_filled(used, 4.0, ACCENT);
        }
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            label,
            egui::FontId::monospace(12.0),
            INK,
        );
    }

    /// The evidence panel: the measurement behind the selected path, in the CLI's words.
    fn show_explain(&mut self, ui: &mut egui::Ui) {
        let Some(path) = self.selection.clone() else {
            return;
        };
        ui.heading("evidence");
        ui.separator();
        let record = self
            .index
            .as_ref()
            .and_then(|index| index.explain_path(&path).ok().flatten());
        let Some(record) = record else {
            ui.label("no longer in the index — refresh it");
            if ui.button("Clear selection").clicked() {
                self.selection = None;
            }
            return;
        };

        ui.monospace(&record.path);
        ui.label(format!("kind  {}", record.kind));
        if record.kind == "file" {
            ui.label(format!("reported  {}", human_bytes_exact(record.logical)));
        }
        ui.label(format!(
            "occupies  {} ({})",
            human_bytes_exact(record.allocated),
            if record.provenance.allocation_measured() {
                "measured"
            } else {
                "reported, not measured"
            }
        ));
        if record.links > 1 {
            ui.label(format!(
                "identity  {} names share this data; counted once",
                record.links
            ));
        }
        match (record.stream_count, record.provenance) {
            (0, hfs_core::FileProvenance::Full | hfs_core::FileProvenance::NoIdentity) => {
                ui.label("streams  none — measured");
            }
            (0, _) => {
                ui.label("streams  unknown — never enumerated");
            }
            (count, _) => {
                ui.label(format!(
                    "streams  {} across {count} — measured on allocation",
                    human_bytes_exact(record.stream_bytes)
                ));
            }
        }
        if record.kind == "file" {
            ui.label(format!(
                "modified  {}",
                human_age(record.modified, windows_ticks_now())
            ));
        }
        ui.separator();
        ui.label(format!("measured by {}", record.provenance.describe()));
        ui.separator();
        ui.horizontal(|ui| {
            // Navigation only: Explorer opens showing the file. Nothing is created,
            // written or deleted, so the read-only invariant holds — the volume is
            // untouched by looking at it through another window.
            if ui.button("Reveal in Explorer").clicked() {
                let _ = std::process::Command::new("explorer")
                    .arg(format!("/select,{path}"))
                    .spawn();
            }
            if ui.button("Copy path").clicked() {
                ui.ctx().copy_text(path.clone());
            }
            if ui.button("Close").clicked() {
                self.selection = None;
            }
        });
    }
}
