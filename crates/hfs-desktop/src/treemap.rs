//! Squarified treemap layout: pure geometry, no I/O, tested anywhere.
//!
//! Rectangles are sized by *allocated* bytes — the storage a file actually occupies, not
//! the size it reports. Every other treemap in the genre sizes by logical size, which is
//! precisely the optimistic lie this project exists to fix: a sparse 100 GB image would
//! fill the screen while occupying 2 GB of disk. Here a tile's area is a measurement.
//!
//! The algorithm is Bruls–Huizing–van Wijk squarification: rows of tiles laid along the
//! short side, each new tile joining the row only while the worst aspect ratio does not
//! worsen. Good ratios keep small tiles clickable, which is the whole point of the view.

use egui::{Rect, Vec2, pos2};

/// One laid-out tile: where it is, and which input item it shows.
#[derive(Debug, Clone)]
pub struct Tile {
    pub rect: Rect,
    pub item: usize,
}

/// Lay out `weights` inside `rect`, largest first for the best ratios.
///
/// Zero weights are skipped (they would be invisible slivers claiming space). The returned
/// tiles cover `rect` up to floating-point dust; areas are proportional to weights.
pub fn layout(weights: &[u64], rect: Rect) -> Vec<Tile> {
    let total: u64 = weights.iter().sum();
    if total == 0 || rect.area() <= 0.0 || !rect.is_finite() {
        return Vec::new();
    }
    let scale = rect.area() / total as f32;
    let mut items: Vec<(usize, f32)> = weights
        .iter()
        .enumerate()
        .filter(|(_, weight)| **weight > 0)
        .map(|(index, &weight)| (index, weight as f32 * scale))
        .collect();
    // Largest first: big tiles anchor the layout and small ones stay readable.
    items.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let mut tiles = Vec::with_capacity(items.len());
    let mut rest = rect;
    let mut row: Vec<(usize, f32)> = Vec::new();

    let mut queue: std::collections::VecDeque<(usize, f32)> = items.into();
    while let Some(&(index, area)) = queue.front() {
        let short = rest.width().min(rest.height());
        if short <= 0.0 {
            break;
        }
        row.push((index, area));
        queue.pop_front();
        // Keep rowing while the next tile would not worsen the worst ratio; otherwise lay
        // the row down and start a fresh one against the remaining space.
        let worsens = queue.front().is_some_and(|&(ni, na)| {
            let mut trial = row.clone();
            trial.push((ni, na));
            worst(&trial, short) > worst(&row, short)
        });
        if worsens || queue.is_empty() {
            layout_row(&row, &mut rest, &mut tiles);
            row.clear();
        }
    }
    if !row.is_empty() {
        layout_row(&row, &mut rest, &mut tiles);
    }
    tiles
}

/// The worst aspect ratio in a row against side `w`: lower is better.
fn worst(row: &[(usize, f32)], w: f32) -> f32 {
    let sum: f32 = row.iter().map(|(_, area)| area).sum();
    if sum <= 0.0 || w <= 0.0 {
        return f32::INFINITY;
    }
    let max = row.iter().map(|(_, area)| *area).fold(0.0f32, f32::max);
    let min = row
        .iter()
        .map(|(_, area)| *area)
        .fold(f32::INFINITY, f32::min);
    if min <= 0.0 {
        return f32::INFINITY;
    }
    (w * w * max / (sum * sum)).max(sum * sum / (w * w * min))
}

/// Lay one row as a strip across the long side, tiles proportional to area within it.
fn layout_row(row: &[(usize, f32)], rect: &mut Rect, tiles: &mut Vec<Tile>) {
    let total: f32 = row.iter().map(|(_, area)| area).sum();
    if total <= 0.0 {
        return;
    }
    if rect.width() >= rect.height() {
        let height = total / rect.width();
        let mut x = rect.min.x;
        for (item, area) in row {
            let width = area / height;
            tiles.push(Tile {
                rect: Rect::from_min_size(pos2(x, rect.min.y), Vec2::new(width, height)),
                item: *item,
            });
            x += width;
        }
        rect.min.y += height;
    } else {
        let width = total / rect.height();
        let mut y = rect.min.y;
        for (item, area) in row {
            let height = area / width;
            tiles.push(Tile {
                rect: Rect::from_min_size(pos2(rect.min.x, y), Vec2::new(width, height)),
                item: *item,
            });
            y += height;
        }
        rect.min.x += width;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::pos2;

    fn rect(w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(0.0, 0.0), Vec2::new(w, h))
    }

    #[test]
    fn empty_and_zero_weights_lay_out_nothing() {
        assert!(layout(&[], rect(100.0, 100.0)).is_empty());
        assert!(layout(&[0, 0, 0], rect(100.0, 100.0)).is_empty());
        assert!(layout(&[10], rect(0.0, 100.0)).is_empty());
    }

    #[test]
    fn areas_are_proportional_to_weights() {
        let tiles = layout(&[3, 1], rect(200.0, 100.0));
        assert_eq!(tiles.len(), 2);
        let areas: Vec<f32> = tiles.iter().map(|tile| tile.rect.area()).collect();
        let total: f32 = areas.iter().sum();
        assert!((total - 20_000.0).abs() < 1.0, "tiles cover the rect");
        assert!(
            (areas[0] / areas[1] - 3.0).abs() < 0.05,
            "3:1 weights give 3:1 areas, got {}",
            areas[0] / areas[1]
        );
    }

    #[test]
    fn tiles_stay_inside_and_do_not_overlap() {
        let tiles = layout(&[50, 30, 15, 10, 8, 5, 3, 2, 1], rect(400.0, 300.0));
        assert_eq!(tiles.len(), 9);
        let outer = rect(400.0, 300.0);
        for tile in &tiles {
            assert!(
                outer.contains_rect(tile.rect),
                "tile escapes the rect: {:?}",
                tile.rect
            );
        }
        // Pairwise disjoint interiors (edges may touch).
        for (i, a) in tiles.iter().enumerate() {
            for b in tiles.iter().skip(i + 1) {
                let overlap = a.rect.intersect(b.rect);
                assert!(
                    overlap.area() < 1.0,
                    "tiles {i} and ? overlap by {}",
                    overlap.area()
                );
            }
        }
        let total: f32 = tiles.iter().map(|tile| tile.rect.area()).sum();
        assert!(
            (total - 120_000.0).abs() / 120_000.0 < 0.01,
            "coverage within 1%, got {total}"
        );
    }

    #[test]
    fn one_item_takes_the_whole_rect() {
        let tiles = layout(&[42], rect(100.0, 50.0));
        assert_eq!(tiles.len(), 1);
        assert!((tiles[0].rect.area() - 5_000.0).abs() < 0.5);
    }
}
