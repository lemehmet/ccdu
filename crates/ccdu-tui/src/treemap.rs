//! Squarified treemap layout.
//!
//! Areas are proportional to size, and the algorithm (Bruls, Huizing and van Wijk) keeps the
//! rectangles as close to square as it can — long thin slivers are hard to compare by eye and hard
//! to label, which is the entire point of drawing this instead of reading the list.
//!
//! Geometry only: this module knows nothing about ratatui and is tested directly.

use ccdu_core::model::NodeId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tile {
    pub id: NodeId,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl Tile {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    pub fn area(&self) -> u32 {
        self.width as u32 * self.height as u32
    }
}

/// A rectangle still to be filled, in cells.
#[derive(Clone, Copy, Debug)]
struct Free {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

impl Free {
    fn shorter_side(&self) -> u16 {
        self.width.min(self.height)
    }
}

/// Lay `items` out in a `width` by `height` area at the origin `(x, y)`.
///
/// Items are taken largest first; anything that would round to nothing is dropped rather than
/// drawn as a zero-width sliver.
pub fn squarify(items: &[(NodeId, u64)], x: u16, y: u16, width: u16, height: u16) -> Vec<Tile> {
    let mut sorted: Vec<(NodeId, u64)> =
        items.iter().copied().filter(|(_, size)| *size > 0).collect();
    sorted.sort_by_key(|&(id, size)| (std::cmp::Reverse(size), id));

    let total: u128 = sorted.iter().map(|(_, size)| *size as u128).sum();
    if sorted.is_empty() || total == 0 || width == 0 || height == 0 {
        return Vec::new();
    }

    let mut tiles = Vec::with_capacity(sorted.len());
    let mut free = Free { x, y, width, height };
    let mut remaining = total;
    let mut index = 0;

    while index < sorted.len() && free.width > 0 && free.height > 0 {
        // Cells still available for everything not yet placed.
        let cells = free.width as u128 * free.height as u128;

        // Grow the current row while the worst rectangle in it keeps getting squarer.
        let mut row_end = index;
        let mut row_sum: u128 = 0;
        let mut best = f64::MAX;
        while row_end < sorted.len() {
            let next_sum = row_sum + sorted[row_end].1 as u128;
            let ratio = worst_ratio(&sorted[index..=row_end], next_sum, cells, remaining, &free);
            if row_end > index && ratio > best {
                break;
            }
            best = ratio;
            row_sum = next_sum;
            row_end += 1;
        }

        place_row(&sorted[index..row_end], row_sum, cells, remaining, &mut free, &mut tiles);
        remaining -= row_sum;
        index = row_end;
    }

    tiles
}

/// Worst aspect ratio in a candidate row, where 1.0 is a perfect square.
fn worst_ratio(
    row: &[(NodeId, u64)],
    row_sum: u128,
    cells: u128,
    remaining: u128,
    free: &Free,
) -> f64 {
    if row_sum == 0 || remaining == 0 {
        return f64::MAX;
    }
    let side = free.shorter_side() as f64;
    // Cells this row will occupy, and therefore how thick it is.
    let row_cells = row_sum as f64 * cells as f64 / remaining as f64;
    let thickness = row_cells / side;
    if thickness <= 0.0 {
        return f64::MAX;
    }

    let mut worst: f64 = 1.0;
    for &(_, size) in row {
        let length = size as f64 * cells as f64 / remaining as f64 / thickness;
        if length <= 0.0 {
            return f64::MAX;
        }
        worst = worst.max((thickness / length).max(length / thickness));
    }
    worst
}

/// Cut a strip off the free rectangle's shorter side and divide it among `row`.
fn place_row(
    row: &[(NodeId, u64)],
    row_sum: u128,
    cells: u128,
    remaining: u128,
    free: &mut Free,
    tiles: &mut Vec<Tile>,
) {
    if row.is_empty() || row_sum == 0 || remaining == 0 {
        return;
    }
    let vertical = free.width <= free.height;
    let span = if vertical { free.width } else { free.height };

    // Thickness in whole cells, at least one so the row is visible at all.
    let exact = row_sum as f64 * cells as f64 / remaining as f64 / span.max(1) as f64;
    let thickness =
        (exact.round() as u16).clamp(1, if vertical { free.height } else { free.width });

    let mut offset = 0u16;
    let mut placed: u128 = 0;
    for (i, &(id, size)) in row.iter().enumerate() {
        // The last item takes the remainder, so rounding never leaves a gap or overruns.
        let length = if i + 1 == row.len() {
            span - offset
        } else {
            placed += size as u128;
            let want = (placed as f64 * span as f64 / row_sum as f64).round() as u16;
            want.saturating_sub(offset).min(span - offset)
        };
        if length > 0 {
            tiles.push(if vertical {
                Tile { id, x: free.x + offset, y: free.y, width: length, height: thickness }
            } else {
                Tile { id, x: free.x, y: free.y + offset, width: thickness, height: length }
            });
        }
        offset += length;
    }

    if vertical {
        free.y += thickness;
        free.height = free.height.saturating_sub(thickness);
    } else {
        free.x += thickness;
        free.width = free.width.saturating_sub(thickness);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(sizes: &[u64]) -> Vec<(NodeId, u64)> {
        sizes.iter().enumerate().map(|(i, &s)| (i as NodeId, s)).collect()
    }

    /// Every cell belongs to exactly one tile, and no tile leaves the area.
    fn assert_tiles_partition(tiles: &[Tile], x: u16, y: u16, width: u16, height: u16) {
        let mut seen = vec![false; width as usize * height as usize];
        for tile in tiles {
            assert!(tile.width > 0 && tile.height > 0, "zero-sized tile {tile:?}");
            assert!(
                tile.x >= x
                    && tile.y >= y
                    && tile.x + tile.width <= x + width
                    && tile.y + tile.height <= y + height,
                "tile {tile:?} escapes the {width}x{height} area at ({x},{y})"
            );
            for cy in tile.y..tile.y + tile.height {
                for cx in tile.x..tile.x + tile.width {
                    let index = (cy - y) as usize * width as usize + (cx - x) as usize;
                    assert!(!seen[index], "tiles overlap at ({cx},{cy})");
                    seen[index] = true;
                }
            }
        }
        assert!(seen.iter().all(|&covered| covered), "part of the area was left blank");
    }

    #[test]
    fn tiles_cover_the_area_exactly() {
        let tiles = squarify(&items(&[6, 6, 4, 3, 2, 2, 1]), 0, 0, 40, 12);
        assert_eq!(tiles.len(), 7);
        assert_tiles_partition(&tiles, 0, 0, 40, 12);
    }

    #[test]
    fn tiles_respect_an_offset_origin() {
        let tiles = squarify(&items(&[5, 3, 2]), 7, 4, 20, 9);
        assert_tiles_partition(&tiles, 7, 4, 20, 9);
    }

    #[test]
    fn area_follows_size() {
        let tiles = squarify(&items(&[100, 50, 25]), 0, 0, 60, 20);
        let area = |id: NodeId| tiles.iter().find(|t| t.id == id).unwrap().area() as f64;

        // Proportions hold approximately; whole cells cannot divide exactly.
        assert!((area(0) / area(1) - 2.0).abs() < 0.35, "{:?}", tiles);
        assert!(area(0) > area(1) && area(1) > area(2));
    }

    #[test]
    fn rectangles_stay_reasonably_square() {
        let tiles = squarify(&items(&[10; 16]), 0, 0, 40, 20);
        assert_tiles_partition(&tiles, 0, 0, 40, 20);

        // The whole point of squarifying: no long thin slivers among equal-sized items.
        for tile in &tiles {
            let ratio = tile.width as f64 / tile.height as f64;
            assert!(ratio > 0.15 && ratio < 7.0, "sliver {tile:?}");
        }
    }

    #[test]
    fn one_item_takes_everything() {
        let tiles = squarify(&items(&[42]), 0, 0, 10, 5);
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0], Tile { id: 0, x: 0, y: 0, width: 10, height: 5 });
    }

    #[test]
    fn empty_input_and_empty_areas_produce_nothing() {
        assert!(squarify(&[], 0, 0, 20, 10).is_empty());
        assert!(squarify(&items(&[1, 2]), 0, 0, 0, 10).is_empty());
        assert!(squarify(&items(&[1, 2]), 0, 0, 20, 0).is_empty());
        assert!(squarify(&items(&[0, 0]), 0, 0, 20, 10).is_empty(), "zero-sized items");
    }

    #[test]
    fn more_items_than_cells_still_partitions_cleanly() {
        // 200 items into 20 cells: most cannot be drawn, and the ones that are must still tile.
        let tiles = squarify(&items(&vec![1; 200]), 0, 0, 5, 4);
        assert!(!tiles.is_empty());
        assert_tiles_partition(&tiles, 0, 0, 5, 4);
    }

    #[test]
    fn a_single_row_of_cells_is_handled() {
        let tiles = squarify(&items(&[3, 2, 1]), 0, 0, 30, 1);
        assert_tiles_partition(&tiles, 0, 0, 30, 1);
        assert!(tiles.iter().all(|t| t.height == 1));
    }

    #[test]
    fn a_dominant_item_does_not_starve_the_rest_of_the_area() {
        // One huge item and several tiny ones: the tiny ones may be dropped, but whatever is drawn
        // must still cover the area with no gaps.
        let tiles = squarify(&items(&[1_000_000, 1, 1, 1]), 0, 0, 30, 10);
        assert_tiles_partition(&tiles, 0, 0, 30, 10);
        let biggest = tiles.iter().find(|t| t.id == 0).unwrap();
        assert!(biggest.area() > 200, "the dominant item should dominate: {biggest:?}");
    }

    #[test]
    fn layout_is_deterministic_for_equal_sizes() {
        let a = squarify(&items(&[5, 5, 5, 5]), 0, 0, 20, 10);
        let b = squarify(&items(&[5, 5, 5, 5]), 0, 0, 20, 10);
        assert_eq!(a, b, "ties must break the same way every frame or the map flickers");
    }

    #[test]
    fn lookup_by_position_finds_the_right_tile() {
        let tiles = squarify(&items(&[8, 4, 2, 1]), 0, 0, 24, 8);
        for tile in &tiles {
            let found = tiles.iter().find(|t| t.contains(tile.x, tile.y)).unwrap();
            assert_eq!(found.id, tile.id);
        }
    }
}
