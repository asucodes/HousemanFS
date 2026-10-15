//! The browse screen: largest files, directories and extensions as tables.
//!
//! Rows are full-width selectable lines in monospace: sizes right-aligned, annotations
//! after. Clicking a row selects the path, and the evidence panel explains it. Nothing
//! here acts on the selection — browsing shows, the panel explains, and that is the whole
//! extent of what this screen can do.

use hfs_core::format::{human_age, human_bytes, windows_ticks_now};
use hfs_index::{DirHit, FileHit};

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
                header(
                    ui,
                    &[("Size", 0), ("Age", 1), ("Path", 2)],
                    &mut self.file_sort,
                );
                let mut hits = self.files.clone();
                sort_files(&mut hits, self.file_sort);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for hit in hits {
                        let mut line = format!(
                            "{:>12}  {:>14}  {}",
                            human_bytes(hit.allocated),
                            human_age(hit.modified, windows_ticks_now()),
                            hit.path
                        );
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
                header(
                    ui,
                    &[("Size", 0), ("Files", 1), ("Path", 2)],
                    &mut self.dir_sort,
                );
                let mut hits = self.dirs.clone();
                sort_dirs(&mut hits, self.dir_sort);
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for hit in hits {
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

/// A sort header: the active column shows its direction, clicking re-sorts. State is
/// (column, ascending) — ascending starts false because every listing here leads with
/// its largest, and the header only reverses that.
fn header(ui: &mut egui::Ui, columns: &[(&str, u8)], sort: &mut (u8, bool)) {
    ui.horizontal(|ui| {
        for (label, column) in columns {
            let marker = if sort.0 == *column {
                if sort.1 { " ▲" } else { " ▼" }
            } else {
                ""
            };
            if ui.small_button(format!("{label}{marker}")).clicked() {
                if sort.0 == *column {
                    sort.1 = !sort.1;
                } else {
                    *sort = (*column, false);
                }
            }
        }
        ui.small("age unknown sorts last — unmeasured is not old");
    });
    ui.separator();
}

fn sort_files(hits: &mut [FileHit], sort: (u8, bool)) {
    match sort {
        (1, asc) => {
            // Unknown timestamps sort last in both directions: unmeasured is not old.
            hits.sort_by(|a, b| match (a.modified == 0, b.modified == 0) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => {
                    if asc {
                        a.modified.cmp(&b.modified)
                    } else {
                        b.modified.cmp(&a.modified)
                    }
                }
            });
        }
        (2, asc) => {
            if asc {
                hits.sort_by(|a, b| a.path.cmp(&b.path));
            } else {
                hits.sort_by(|a, b| b.path.cmp(&a.path));
            }
        }
        (_, asc) => {
            if asc {
                hits.sort_by_key(|hit| hit.allocated);
            } else {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.allocated));
            }
        }
    }
}

fn sort_dirs(hits: &mut [DirHit], sort: (u8, bool)) {
    match sort {
        (1, asc) => {
            if asc {
                hits.sort_by_key(|hit| hit.files);
            } else {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.files));
            }
        }
        (2, asc) => {
            if asc {
                hits.sort_by(|a, b| a.path.cmp(&b.path));
            } else {
                hits.sort_by(|a, b| b.path.cmp(&a.path));
            }
        }
        (_, asc) => {
            if asc {
                hits.sort_by_key(|hit| hit.allocated);
            } else {
                hits.sort_by_key(|hit| std::cmp::Reverse(hit.allocated));
            }
        }
    }
}
