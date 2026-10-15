//! The duplicates screen: same bytes under different names, proven by hash.
//!
//! Size groups are suspicion; Blake3 is the verdict. Each confirmed group shows what its
//! copies would occupy reduced to one — a potential, not a promise, because freeing
//! requires deleting and this tool does not delete. Members that share one dataset
//! already (hardlinks) are flagged per member, so the group total never misleads.
//!
//! Hashing reads file contents, which is read-only: the invariant bans writes, not
//! measurement. Unreadable files are excluded from contention and counted, never guessed
//! about.

use hfs_core::format::human_bytes;

use super::browse::row;
use crate::App;
use crate::dupes::DupWorker;

const PRESETS: &[(&str, u64)] = &[
    ("any size", 0),
    ("at least 1 MB", 1 << 20),
    ("at least 10 MB", 10 << 20),
    ("at least 100 MB", 100 << 30),
];

impl App {
    pub fn show_duplicates(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Duplicates");
            ui.small("same size is suspicion — same hash is the verdict");
        });
        ui.separator();

        let running = self.dup_worker.is_some();
        ui.add_enabled_ui(!running && self.index.is_some(), |ui| {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt("dup-min")
                    .selected_text(PRESETS[self.dup_min].0)
                    .show_ui(ui, |ui| {
                        for (index, (label, _)) in PRESETS.iter().enumerate() {
                            ui.selectable_value(&mut self.dup_min, index, *label);
                        }
                    });
                if ui.button("Find duplicates").clicked() {
                    self.dup_error = None;
                    self.dup_report = None;
                    self.dup_progress = (0, 0);
                    self.dup_worker = Some(DupWorker::start(
                        self.db_path.clone(),
                        PRESETS[self.dup_min].1,
                    ));
                }
            });
        });
        if self.index.is_none() {
            ui.small("Scan a folder first — there is nothing to compare.");
            return;
        }

        if running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.monospace(format!(
                    "{} files hashed, {} read so far",
                    self.dup_progress.0,
                    human_bytes(self.dup_progress.1)
                ));
                if ui.button("Cancel").clicked() {
                    if let Some(worker) = &self.dup_worker {
                        worker.cancel();
                    }
                }
            });
            ui.small("Prefix hashes first, full hashes only within ties. Unreadable files are excluded, not assumed.");
        }
        if let Some(error) = self.dup_error.clone() {
            ui.colored_label(egui::Color32::DARK_RED, error);
        }
        let Some(report) = self.dup_report.as_ref() else {
            if !running {
                ui.small("Hashing starts where the bytes are: candidates share a reported size, and groups are capped at 200.");
            }
            return;
        };

        ui.small(format!(
            "{} size groups checked, {} confirmed, {} unreadable skipped, {} read",
            report.groups_checked,
            report.groups.len(),
            report.skipped,
            human_bytes(report.bytes_read)
        ));
        if report.groups.is_empty() {
            ui.label("No confirmed duplicates. Same sizes, different bytes — or everything shares storage already.");
            return;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for group in &report.groups {
                let copies = group.members.len();
                egui::CollapsingHeader::new(format!(
                    "{} copies × {} — potential {} if reduced to one",
                    copies,
                    human_bytes(group.members[0].allocated),
                    human_bytes(group.reclaimable)
                ))
                .default_open(false)
                .show(ui, |ui| {
                    for member in &group.members {
                        let mut line =
                            format!("{:>12}  {}", human_bytes(member.allocated), member.path);
                        if member.links > 1 {
                            line.push_str(&format!(
                                "  [{} names share this copy — deleting one frees nothing]",
                                member.links
                            ));
                        }
                        if row(ui, &line, self.selection.as_deref() == Some(&member.path)) {
                            self.selection = Some(member.path.clone());
                        }
                    }
                });
            }
        });
        ui.small("Potential, not promise: freeing requires deleting, which this tool does not do. Verify (measured free-space delta) before acting anywhere.");
    }

    /// Poll the duplicate worker once per frame, like the scan worker.
    pub fn poll_dupes(&mut self, context: &egui::Context) {
        let finished = if let Some(worker) = self.dup_worker.as_mut() {
            let (progress, done) = worker.poll();
            if let Some((members, bytes)) = progress {
                self.dup_progress = (members, bytes);
            }
            if let Some(result) = done {
                match result {
                    Ok(report) => {
                        self.dup_report = Some(report);
                        self.dup_error = None;
                    }
                    Err(error) => self.dup_error = Some(error),
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
            self.dup_worker = None;
        }
    }
}
