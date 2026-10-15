//! The start screen: pick a folder, pick an index file, scan.

use crate::scan::ScanWorker;
use crate::{App, Screen};

impl App {
    pub fn show_home(&mut self, ui: &mut egui::Ui) {
        ui.heading("Where did the space go?");
        ui.label("Pick a folder. The tool walks it read-only, accounts every byte it can measure, and says plainly what it could not.");
        ui.separator();

        ui.horizontal(|ui| {
            ui.label("Folder");
            ui.text_edit_singleline(&mut self.scan_path);
            if ui.button("Browse…").clicked() {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    self.scan_path = folder.display().to_string();
                }
            }
        });
        ui.horizontal(|ui| {
            ui.label("Index ");
            ui.text_edit_singleline(&mut self.db_path);
            if ui.button("Browse…").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("HousemanFS index", &["db"])
                    .pick_file()
                {
                    self.db_path = file.display().to_string();
                }
            }
        });

        let scanning = self.worker.is_some();
        ui.add_enabled_ui(!scanning && !self.scan_path.is_empty(), |ui| {
            ui.horizontal(|ui| {
                if ui.button("Scan (replace index)").clicked() {
                    self.start_scan(true);
                }
                // Refresh needs an index of the same volume; the worker refuses a foreign
                // one before walking, so the button is safe to offer blind.
                if ui.button("Refresh (update index)").clicked() {
                    self.start_scan(false);
                }
            });
        });
        if scanning {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.monospace(format!(
                    "{} files, {} directories so far",
                    self.progress.0, self.progress.1
                ));
            });
            ui.label("A whole volume takes minutes. The walk measures on allocation size, counts hardlink groups once, and enumerates named streams — thoroughness is the cost.");
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Open existing index…").clicked() {
                if let Some(file) = rfd::FileDialog::new()
                    .add_filter("HousemanFS index", &["db"])
                    .pick_file()
                {
                    let path = file.display().to_string();
                    match self.open_database(&path) {
                        Ok(()) => {
                            self.db_path = path;
                            self.screen = Screen::Dashboard;
                        }
                        Err(error) => self.scan_error = Some(error),
                    }
                }
            }
        });
        ui.small("An index is derived data: rebuildable, never a source of truth — and a map of your machine, so treat the file as private.");
    }

    pub(crate) fn start_scan(&mut self, full: bool) {
        self.scan_error = None;
        self.notice = None;
        self.progress = (0, 0);
        self.worker = Some(ScanWorker::start(
            self.scan_path.clone(),
            self.db_path.clone(),
            full,
        ));
    }
}
