//! The map: a treemap of the largest directory subtrees.
//!
//! The genre convention — WinDirStat, SpaceSniffer — sizes tiles by reported size. This
//! map sizes by *allocated* bytes instead, because a tile's area should be storage, not a
//! press release: the sparse VM image that fills other treemaps shrinks here to what it
//! actually occupies. Clicking a tile selects the directory for the evidence panel.

use hfs_core::format::human_bytes;

use crate::treemap::layout;
use crate::{App, INK, PANEL};

impl App {
    pub fn show_map(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Treemap");
            ui.small("areas ∝ allocated bytes · click a tile for evidence");
        });
        ui.separator();

        let items: Vec<(String, u64)> = self
            .dirs
            .iter()
            .take(40)
            .map(|hit| (hit.path.clone(), hit.allocated))
            .collect();
        if items.is_empty() {
            ui.label("nothing to map — scan a folder first");
            return;
        }

        let available = ui.available_size();
        let (response, painter) = ui.allocate_painter(available, egui::Sense::click_and_drag());
        let rect = response.rect;
        painter.rect_filled(rect, 6.0, PANEL);

        let weights: Vec<u64> = items.iter().map(|(_, bytes)| *bytes).collect();
        let tiles = layout(&weights, rect.shrink(6.0));
        let max = weights.iter().max().copied().unwrap_or(1) as f32;

        for tile in &tiles {
            let (path, bytes) = &items[tile.item];
            // Larger tiles read darker: the eye finds the biggest claim first.
            let t = (*bytes as f32 / max).sqrt();
            let fill = egui::Color32::from_rgb(
                lerp(0xE6, 0x18, t),
                lerp(0xF1, 0x5F, t),
                lerp(0xFB, 0xA5, t),
            );
            painter.rect_filled(tile.rect, 3.0, fill);
            painter.rect_stroke(
                tile.rect,
                3.0,
                egui::Stroke::new(1.0_f32, PANEL),
                egui::StrokeKind::Inside,
            );
            if tile.rect.width() > 48.0 && tile.rect.height() > 30.0 {
                let tail = path.rsplit(['\\', '/']).next().unwrap_or(path);
                painter.text(
                    tile.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{tail}\n{}", human_bytes(*bytes)),
                    egui::FontId::monospace(11.0),
                    INK,
                );
            }
        }

        if let Some(position) = response.hover_pos() {
            if let Some(tile) = tiles.iter().find(|tile| tile.rect.contains(position)) {
                let (path, bytes) = &items[tile.item];
                egui::show_tooltip_at_pointer(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new("tile-tip"),
                    |ui| {
                        ui.monospace(path);
                        ui.monospace(human_bytes(*bytes));
                    },
                );
            }
        }
        if response.clicked() {
            if let Some(position) = response.hover_pos() {
                if let Some(tile) = tiles.iter().find(|tile| tile.rect.contains(position)) {
                    self.selection = Some(items[tile.item].0.clone());
                }
            }
        }
    }
}

fn lerp(light: u8, dark: u8, t: f32) -> u8 {
    (light as f32 + (dark as f32 - light as f32) * t.clamp(0.0, 1.0)) as u8
}
