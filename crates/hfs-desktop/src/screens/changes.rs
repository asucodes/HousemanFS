//! The changes screen: what moved since the last refresh.
//!
//! Every refresh writes what it added, removed, modified and moved into the change log,
//! capped — counts stay exact past the cap, paths do not. This screen reads that log, so
//! "what changed on my disk" is a question with an answer, not a fresh scan. Paths, not
//! rows: re-query an entry for current figures, because the file may have moved again.

use hfs_core::format::{human_age, windows_ticks_now};

use super::browse::row;
use crate::App;

impl App {
    pub fn show_changes(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Changes");
            ui.small("since the last refresh — paths, re-query for current figures");
            if ui.button("Reload").clicked() {
                self.load_changes();
            }
        });
        ui.separator();

        if self.changes.is_empty() {
            ui.label("No recorded changes. Refresh after changing files and the delta lands here.");
            ui.small("The log holds the last refresh only, capped at 500 paths — counts in the refresh report stay exact past that.");
            return;
        }
        let now = windows_ticks_now();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for change in self.changes.clone() {
                let line = format!(
                    "{:>9}  {:>14}  {}",
                    change.kind,
                    human_age(change.at, now),
                    change.path
                );
                if row(ui, &line, self.selection.as_deref() == Some(&change.path)) {
                    self.selection = Some(change.path);
                }
            }
        });
    }

    /// Load the change log for the open index. Called when the screen opens and after
    /// every refresh, never per frame.
    pub(crate) fn load_changes(&mut self) {
        self.changes = self
            .index
            .as_ref()
            .and_then(|index| index.recent_changes(200).ok())
            .unwrap_or_default();
    }
}
