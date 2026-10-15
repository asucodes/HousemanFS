//! The dashboard: the volume at a glance and the residual, decomposed.
//!
//! One screen answers "where did my space go": capacity against use, attributed against
//! unaccounted, and every component the walk could name. The verdict line judges the gap
//! by the shared tolerance — the same finding the CLI prints, read from the same struct.

use hfs_core::format::human_bytes_exact;
use hfs_core::{RESIDUAL_TOLERANCE, Reconciliation};

use crate::{ACCENT, App, INK, Screen};

impl App {
    pub fn show_dashboard(&mut self, ui: &mut egui::Ui) {
        let Some(accounting) = &self.accounting else {
            ui.heading("Index overview");
            ui.label(format!(
                "{} files occupying {}",
                self.summary.files,
                human_bytes_exact(self.summary.allocated)
            ));
            ui.small("Volume reconciliation needs a volume-rooted index; this one covers a subtree, so there is no residual to judge.");
            return;
        };

        ui.heading(format!("Volume {}", accounting.volume.root));
        let scanned_as = if self.meta.root.is_empty() {
            "unknown".to_string()
        } else if self.meta.elevated {
            "elevated".to_string()
        } else {
            "standard user".to_string()
        };
        ui.label(format!(
            "{} · {} clusters · scanned as {}",
            accounting.volume.filesystem,
            human_bytes_exact(u64::from(accounting.volume.cluster_bytes)),
            scanned_as
        ));
        ui.separator();

        let total = accounting.accounting.total;
        let used = accounting.accounting.used();
        Self::usage_bar(
            ui,
            used,
            total,
            format!(
                "{} of {} used",
                human_bytes_exact(used),
                human_bytes_exact(total)
            ),
        );
        ui.small(format!(
            "{} free",
            human_bytes_exact(accounting.accounting.free)
        ));
        ui.separator();

        let attributed = accounting.accounting.attributed();
        Self::usage_bar(
            ui,
            attributed,
            used.max(1),
            format!("{} of used space attributed", human_bytes_exact(attributed)),
        );
        let residual = accounting.accounting.residual();
        let ratio = accounting.accounting.residual_ratio();
        match accounting.reconciliation {
            Reconciliation::Explained { .. } => {
                ui.colored_label(
                    ACCENT,
                    format!(
                        "residual {} ({:.2}% of capacity) — within the {:.0}% tolerance",
                        human_bytes_exact(residual),
                        ratio * 100.0,
                        RESIDUAL_TOLERANCE * 100.0
                    ),
                );
            }
            Reconciliation::Unexplained { .. } => {
                ui.colored_label(
                    egui::Color32::DARK_RED,
                    format!(
                        "residual {} ({:.2}% of capacity) — beyond the {:.0}% tolerance. This is a finding, not an error.",
                        human_bytes_exact(residual),
                        ratio * 100.0,
                        RESIDUAL_TOLERANCE * 100.0
                    ),
                );
            }
        }
        ui.separator();

        ui.heading("Components");
        component_row(
            ui,
            "file data",
            human_bytes_exact(self.summary.allocated),
            "measured on allocation, hardlink groups counted once",
        );
        if self.summary.stream_bytes > 0 {
            component_row(
                ui,
                "named streams",
                human_bytes_exact(self.summary.stream_bytes),
                "measured per stream; directory streams not enumerated",
            );
        }
        component_row(
            ui,
            "directory overhead",
            human_bytes_exact(self.meta.directory_bytes),
            "index blocks outside the master file table",
        );
        if let Some(bytes) = accounting.mft_bytes {
            component_row(
                ui,
                "master file table",
                human_bytes_exact(bytes),
                "measured allocation, not live records",
            );
        } else if let Some(error) = &accounting.metadata_error {
            component_row(
                ui,
                "filesystem metadata",
                format!("query refused: {error}"),
                "the residual absorbs it",
            );
        } else {
            component_row(
                ui,
                "filesystem metadata",
                "not measured — requires an elevated scan".to_string(),
                "the residual absorbs it",
            );
        }
        if accounting.system_measured > 0 {
            component_row(
                ui,
                "NTFS system files",
                format!(
                    "{} across {} files",
                    human_bytes_exact(accounting.system_bytes),
                    accounting.system_measured
                ),
                "measured by name; a walk never sees them",
            );
        }
        let denied = self.meta.denied_directories + self.meta.denied_files;
        if denied > 0 {
            component_row(
                ui,
                "unreadable paths",
                format!("{denied}"),
                "contents entirely unknown — the likeliest home of any gap",
            );
        }
        ui.separator();

        ui.horizontal(|ui| {
            if ui.button("Refresh index").clicked() {
                self.scan_path = self.scan_root.clone();
                self.start_scan(false);
                self.screen = Screen::Home;
            }
            if ui.button("Export JSON…").clicked() {
                self.export_json(ui);
            }
            if ui.button("Browse files").clicked() {
                self.screen = Screen::Browse;
            }
        });
        ui.small("Export writes the versioned JSON snapshot outside the scanned volume. Pick a destination the scan did not cover.");
    }

    fn export_json(&mut self, ui: &mut egui::Ui) {
        let Some(path) = rfd::FileDialog::new()
            .set_file_name("housemanfs-export.json")
            .add_filter("JSON", &["json"])
            .save_file()
        else {
            return;
        };
        let Some(index) = &self.index else {
            return;
        };
        // The extension file must not land inside the scanned tree: an export that the
        // next scan would then index is a feedback loop wearing a feature's clothes.
        let target = path.display().to_string();
        let inside = !self.scan_root.is_empty()
            && (target == self.scan_root
                || target.starts_with(&format!("{}\\", self.scan_root))
                || target.starts_with(&format!("{}/", self.scan_root)));
        if inside {
            self.scan_error =
                Some("export refused: the destination is inside the scanned tree".to_string());
            return;
        }
        match index
            .export_snapshot(200)
            .map_err(|e| e.to_string())
            .and_then(|document| serde_json::to_string_pretty(&document).map_err(|e| e.to_string()))
            .and_then(|text| std::fs::write(&path, text).map_err(|e| e.to_string()))
        {
            Ok(()) => {
                ui.ctx().copy_text(target.clone());
                self.notice = Some(format!("exported (path copied): {target}"));
            }
            Err(error) => self.scan_error = Some(error),
        }
    }
}

fn component_row(ui: &mut egui::Ui, label: &str, value: String, note: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(label).color(INK).strong());
        ui.monospace(value);
        ui.small(note);
    });
}
