//! The browse screen: largest files, directories and extensions as tables.
//!
//! Rows are full-width selectable lines in monospace: sizes right-aligned, annotations
//! after. Clicking a row selects the path, and the evidence panel explains it. Nothing
//! here acts on the selection — browsing shows, the panel explains, and that is the whole
//! extent of what this screen can do.

use hfs_core::format::human_bytes;

use crate::{App, BrowseTab, INK};

impl App {
    pub fn show_browse(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (BrowseTab::Files, "Files"),
                (BrowseTab::Dirs, "Directories"),
                (BrowseTab::Extensions, "Extensions"),
            ] {
                if ui.selectable_label(self.browse_tab == tab, label).clicked() {
                    self.browse_tab = tab;
                }
            }
        });
        ui.separator();

        match self.browse_tab {
            BrowseTab::Files => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for hit in self.files.clone() {
                        let mut line = format!("{:>12}  {}", human_bytes(hit.allocated), hit.path);
                        if hit.links > 1 {
                            line.push_str(&format!("  [{} names share this data]", hit.links));
                        }
                        if hit.stream_count > 0 {
                            line.push_str(&format!(
                                "  [+{} in {} streams]",
                                human_bytes(hit.stream_bytes),
                                hit.stream_count
                            ));
                        }
                        if row(ui, &line, self.selection.as_deref() == Some(&hit.path)) {
                            self.selection = Some(hit.path);
                        }
                    }
                });
            }
            BrowseTab::Dirs => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for hit in self.dirs.clone() {
                        let mut line = format!(
                            "{:>12}  {:>7} files  {}",
                            human_bytes(hit.allocated),
                            hit.files,
                            hit.path
                        );
                        if hit.streams > 0 {
                            line.push_str(&format!("  [+{} in streams]", human_bytes(hit.streams)));
                        }
                        if row(ui, &line, self.selection.as_deref() == Some(&hit.path)) {
                            self.selection = Some(hit.path);
                        }
                    }
                });
            }
            BrowseTab::Extensions => {
                let total = self.summary.allocated.max(1) as f32;
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (ext, allocated, count) in self.exts.clone() {
                        let label = if ext.is_empty() { "(none)" } else { &ext };
                        ui.horizontal(|ui| {
                            ui.monospace(format!("{:>12}", human_bytes(allocated)));
                            // A bar is worth more than a column of numbers when the question
                            // is "what kind of stuff fills this disk".
                            let width = (ui.available_width() - 220.0).max(40.0);
                            let fraction = allocated as f32 / total;
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(width * fraction.max(0.02), 14.0),
                                egui::Sense::hover(),
                            );
                            ui.painter().rect_filled(rect, 3.0, INK);
                            ui.monospace(format!("{count:>7} files  {label}"));
                        });
                    }
                });
            }
        }
        ui.small("Sizes are allocated bytes. A sparse file ranks by what it occupies, never by what it reports.");
    }
}

/// One full-width selectable row. Returns true when clicked.
pub(crate) fn row(ui: &mut egui::Ui, line: &str, selected: bool) -> bool {
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        ui.selectable_label(selected, egui::RichText::new(line).monospace())
            .clicked()
    })
    .inner
}
