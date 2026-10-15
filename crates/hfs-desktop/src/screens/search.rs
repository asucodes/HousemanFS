//! The search screen: name search plus the structured ask builder.
//!
//! Two ways to interrogate the index. Name search is substring matching, literal and
//! immediate. The ask builder composes size, extension, age, sharing and stream clauses
//! into the same typed query the CLI parses — the widgets bound what can be expressed,
//! which is the same honesty as the parser's closed vocabulary, in checkboxes.

use hfs_core::format::{human_age, human_bytes, windows_ticks_now};
use hfs_index::AskQuery;

use super::browse::row;
use crate::App;

const SIZES: &[(&str, Option<u64>)] = &[
    ("any size", None),
    ("larger than 100 MB", Some(100 << 20)),
    ("larger than 1 GB", Some(1 << 30)),
    ("larger than 10 GB", Some(10 << 30)),
];
const AGES: &[(&str, Option<i64>)] = &[
    ("any age", None),
    ("older than 30 days", Some(30)),
    ("older than 1 year", Some(365)),
];
const KINDS: &[(&str, &[&str])] = &[
    ("anything", &[]),
    ("videos", &["mp4", "mkv", "avi", "mov", "wmv", "webm"]),
    ("audio", &["mp3", "flac", "wav", "ogg", "m4a"]),
    ("images", &["jpg", "jpeg", "png", "gif", "bmp", "webp"]),
    ("archives", &["zip", "7z", "rar", "tar", "gz"]),
    ("installers", &["exe", "msi"]),
    ("disk images", &["iso", "img", "vhd", "vhdx"]),
];

impl App {
    pub fn show_search(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Name");
            if ui.text_edit_singleline(&mut self.find_term).changed() {
                self.run_find();
            }
        });
        if !self.find_term.is_empty() {
            if self.find_rows.is_empty() {
                ui.small(format!("nothing matches {:?}", self.find_term));
            } else {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        for hit in self.find_rows.clone() {
                            let line = format!("{:>12}  {}", human_bytes(hit.allocated), hit.path);
                            let selected = self.selection.as_deref() == Some(hit.path.as_str());
                            if row(ui, &line, selected) {
                                self.selection = Some(hit.path);
                            }
                        }
                    });
            }
        }

        ui.separator();
        ui.heading("Ask");
        ui.small("Clauses combine with AND, largest first. Unknown timestamps never match an age clause.");
        egui::Grid::new("ask-builder")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Size");
                egui::ComboBox::from_id_salt("ask-size")
                    .selected_text(SIZES[self.ask_size].0)
                    .show_ui(ui, |ui| {
                        for (index, (label, _)) in SIZES.iter().enumerate() {
                            ui.selectable_value(&mut self.ask_size, index, *label);
                        }
                    });
                ui.end_row();
                ui.label("Kind");
                egui::ComboBox::from_id_salt("ask-kind")
                    .selected_text(KINDS[self.ask_kind].0)
                    .show_ui(ui, |ui| {
                        for (index, (label, _)) in KINDS.iter().enumerate() {
                            ui.selectable_value(&mut self.ask_kind, index, *label);
                        }
                    });
                ui.end_row();
                ui.label("Extension");
                ui.text_edit_singleline(&mut self.ask_ext);
                ui.end_row();
                ui.label("Age");
                egui::ComboBox::from_id_salt("ask-older")
                    .selected_text(AGES[self.ask_older].0)
                    .show_ui(ui, |ui| {
                        for (index, (label, _)) in AGES.iter().enumerate() {
                            ui.selectable_value(&mut self.ask_older, index, *label);
                        }
                    });
                ui.end_row();
                ui.checkbox(&mut self.ask_shared, "shared names only");
                ui.checkbox(&mut self.ask_streams, "named streams only");
                ui.end_row();
            });
        if ui.button("Ask").clicked() {
            self.run_ask();
        }
        if !self.ask_rows.is_empty() {
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for hit in self.ask_rows.clone() {
                    let line = format!(
                        "{:>12}  {:>14}  {}",
                        human_bytes(hit.allocated),
                        human_age(hit.modified, windows_ticks_now()),
                        hit.path
                    );
                    let selected = self.selection.as_deref() == Some(hit.path.as_str());
                    if row(ui, &line, selected) {
                        self.selection = Some(hit.path);
                    }
                }
            });
        }
    }

    fn run_find(&mut self) {
        let term = self.find_term.trim().to_string();
        self.find_rows = if term.is_empty() {
            Vec::new()
        } else if let Some(index) = &self.index {
            index.search_name(&term, 100).unwrap_or_default()
        } else {
            Vec::new()
        };
    }

    fn run_ask(&mut self) {
        const DAY_TICKS: i64 = 86_400 * 10_000_000;
        let Some(index) = &self.index else {
            return;
        };
        let mut exts: Vec<String> = KINDS[self.ask_kind]
            .1
            .iter()
            .map(|s| s.to_string())
            .collect();
        for ext in self.ask_ext.split([',', ' ', ';']) {
            let clean = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if !clean.is_empty() {
                exts.push(clean);
            }
        }
        let query = AskQuery {
            min_allocated: SIZES[self.ask_size].1,
            max_allocated: None,
            exts,
            name: None,
            older_than: AGES[self.ask_older]
                .1
                .map(|days| windows_ticks_now() - days * DAY_TICKS),
            newer_than: None,
            shared_only: self.ask_shared,
            streams_only: self.ask_streams,
            limit: 200,
        };
        self.ask_rows = index.ask(&query).unwrap_or_default();
    }
}
