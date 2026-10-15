//! The map: a treemap of directory subtrees, with drill-down.
//!
//! The genre convention — WinDirStat, SpaceSniffer — sizes tiles by reported size. This
//! map sizes by *allocated* bytes instead, because a tile's area should be storage, not a
//! press release: the sparse VM image that fills other treemaps shrinks here to what it
//! actually occupies.
//!
//! Click selects for the evidence panel; double-click a directory descends, showing its
//! subdirectories beside its largest files — slices of the same roll-up, never fresh
//! estimates. The breadcrumb climbs back out.

use hfs_core::format::human_bytes;

use crate::treemap::layout;
use crate::{App, INK, PANEL};

/// One tile: a directory (descendable) or a file (a leaf).
struct MapTile {
    path: String,
    bytes: u64,
    dir: Option<[u8; 16]>,
}

impl App {
    pub fn show_map(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("Treemap");
            ui.small(
                "areas ∝ allocated bytes · click for evidence, double-click a folder to descend",
            );
        });
        ui.horizontal(|ui| {
            if self.map_stack.is_empty() {
                ui.small("everything");
            } else {
                if ui.small_button("everything").clicked() {
                    self.map_stack.clear();
                }
                for (depth, (_, path)) in self.map_stack.clone().iter().enumerate() {
                    ui.small("›");
                    let tail = path.rsplit(['\\', '/']).next().unwrap_or(path);
                    if ui.small_button(tail).clicked() {
                        self.map_stack.truncate(depth + 1);
                    }
                }
            }
            if !self.map_stack.is_empty() && ui.small_button("⬆ up").clicked() {
                self.map_stack.pop();
            }
        });
        ui.separator();

        let Some(index) = &self.index else {
            ui.label("nothing to map — scan a folder first");
            return;
        };
        let tiles: Vec<MapTile> = match self.map_stack.last() {
            None => index
                .largest_dirs(40)
                .unwrap_or_default()
                .into_iter()
                .map(|hit| MapTile {
                    path: hit.path,
                    bytes: hit.allocated,
                    dir: Some(hit.id),
                })
                .collect(),
            Some((id, _)) => {
                let mut tiles: Vec<MapTile> = index
                    .dirs_under(id, 30)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|hit| MapTile {
                        path: hit.path,
                        bytes: hit.allocated,
                        dir: Some(hit.id),
                    })
                    .collect();
                tiles.extend(
                    index
                        .files_under(id, 20)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|hit| MapTile {
                            path: hit.path,
                            bytes: hit.allocated,
                            dir: None,
                        }),
                );
                tiles.sort_by_key(|tile| std::cmp::Reverse(tile.bytes));
                tiles
            }
        };
        if tiles.is_empty() {
            ui.label("empty — nothing measured beneath here");
            return;
        }

        let available = ui.available_size();
        let (response, painter) = ui.allocate_painter(available, egui::Sense::click());
        let rect = response.rect;
        painter.rect_filled(rect, 6.0, PANEL);

        let weights: Vec<u64> = tiles.iter().map(|tile| tile.bytes).collect();
        let laid = layout(&weights, rect.shrink(6.0));
        let max = weights.iter().max().copied().unwrap_or(1) as f32;

        for tile in &laid {
            let entry = &tiles[tile.item];
            // Larger tiles read darker: the eye finds the biggest claim first.
            let t = (entry.bytes as f32 / max).sqrt();
            let fill = egui::Color32::from_rgb(
                lerp(0xE6, 0x18, t),
                lerp(0xF1, 0x5F, t),
                lerp(0xFB, 0xA5, t),
            );
            let selected = self.selection.as_deref() == Some(entry.path.as_str());
            painter.rect_filled(tile.rect, 3.0, fill);
            painter.rect_stroke(
                tile.rect,
                3.0,
                egui::Stroke::new(if selected { 3.0_f32 } else { 1.0_f32 }, INK),
                egui::StrokeKind::Inside,
            );
            if tile.rect.width() > 48.0 && tile.rect.height() > 30.0 {
                let tail = entry.path.rsplit(['\\', '/']).next().unwrap_or(&entry.path);
                painter.text(
                    tile.rect.center(),
                    egui::Align2::CENTER_CENTER,
                    format!("{tail}\n{}", human_bytes(entry.bytes)),
                    egui::FontId::monospace(11.0),
                    INK,
                );
            }
        }

        if let Some(position) = response.hover_pos() {
            if let Some(tile) = laid.iter().find(|tile| tile.rect.contains(position)) {
                let entry = &tiles[tile.item];
                egui::show_tooltip_at_pointer(
                    ui.ctx(),
                    ui.layer_id(),
                    egui::Id::new("tile-tip"),
                    |ui| {
                        ui.monospace(&entry.path);
                        ui.monospace(human_bytes(entry.bytes));
                        if entry.dir.is_some() {
                            ui.small("double-click to descend");
                        }
                    },
                );
                if response.double_clicked() {
                    if let Some(id) = entry.dir {
                        self.map_stack.push((id, entry.path.clone()));
                        self.selection = None;
                    }
                } else if response.clicked() {
                    self.selection = Some(entry.path.clone());
                }
            }
        }
    }
}

fn lerp(light: u8, dark: u8, t: f32) -> u8 {
    (light as f32 + (dark as f32 - light as f32) * t.clamp(0.0, 1.0)) as u8
}
