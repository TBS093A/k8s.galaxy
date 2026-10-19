//! Screen-space placement for canvas text: markers and labels claim terminal cells,
//! later ones move to a free spot or are dropped instead of overprinting earlier ones.

use ratatui::layout::Rect;

pub struct Placer {
    left: f64,
    right: f64,
    bottom: f64,
    top: f64,
    w: i32,
    h: i32,
    taken: Vec<bool>,
}

impl Placer {
    /// `area` is the canvas widget area including its border, bounds as given to the canvas.
    pub fn new(area: Rect, x_bounds: [f64; 2], y_bounds: [f64; 2]) -> Self {
        let w = area.width.saturating_sub(2).max(1) as i32;
        let h = area.height.saturating_sub(2).max(1) as i32;
        Self {
            left: x_bounds[0],
            right: x_bounds[1],
            bottom: y_bounds[0],
            top: y_bounds[1],
            w,
            h,
            taken: vec![false; (w * h) as usize],
        }
    }

    /// Same mapping ratatui's Canvas uses when it prints labels.
    pub fn cell(&self, x: f64, y: f64) -> (i32, i32) {
        let cx = (x - self.left) * f64::from(self.w - 1) / (self.right - self.left);
        let cy = (self.top - y) * f64::from(self.h - 1) / (self.top - self.bottom);
        (cx.floor() as i32, cy.floor() as i32)
    }

    /// Data coordinates that land in the middle of a cell.
    pub fn point(&self, col: i32, row: i32) -> (f64, f64) {
        let x = self.left + (f64::from(col) + 0.5) * (self.right - self.left) / f64::from(self.w - 1);
        let y = self.top - (f64::from(row) + 0.5) * (self.top - self.bottom) / f64::from(self.h - 1);
        (x, y)
    }

    fn fits(&self, col: i32, row: i32, len: i32) -> bool {
        row >= 0
            && row < self.h
            && col >= 0
            && col + len <= self.w
            && (col..col + len).all(|c| !self.taken[(row * self.w + c) as usize])
    }

    fn claim(&mut self, col: i32, row: i32, len: i32) {
        for c in col..col + len {
            self.taken[(row * self.w + c) as usize] = true;
        }
    }

    /// Reserve cells without drawing there (e.g. a blip that a label must not cover).
    pub fn block(&mut self, x: f64, y: f64) {
        let (col, row) = self.cell(x, y);
        if self.fits(col, row, 1) {
            self.claim(col, row, 1);
        }
    }

    /// One-cell marker at (x, y), nudged outward in rings until a free cell is found.
    pub fn marker(&mut self, x: f64, y: f64, max_radius: i32) -> Option<(f64, f64)> {
        let (col, row) = self.cell(x, y);
        for r in 0..=max_radius {
            for dy in -r..=r {
                for dx in -r..=r {
                    if dx.abs().max(dy.abs()) != r {
                        continue;
                    }
                    let (c, rw) = (col + dx * 2, row + dy);
                    if self.fits(c, rw, 1) {
                        self.claim(c, rw, 1);
                        return Some(self.point(c, rw));
                    }
                }
            }
        }
        None
    }

    /// Label of `len` cells next to the cell holding (x, y): right, left, then above/below.
    pub fn label(&mut self, x: f64, y: f64, len: usize) -> Option<(f64, f64)> {
        let (col, row) = self.cell(x, y);
        let len = len as i32;
        let candidates = [
            (col + 2, row),
            (col - len - 1, row),
            (col + 1, row - 1),
            (col + 1, row + 1),
            (col - len, row - 1),
            (col - len, row + 1),
            (col - len / 2, row - 1),
            (col - len / 2, row + 1),
            (col + 2, row - 2),
            (col + 2, row + 2),
            (col - len - 1, row - 2),
            (col - len - 1, row + 2),
        ];
        let (c, r) = candidates.into_iter().find(|&(c, r)| self.fits(c, r, len))?;
        self.claim(c, r, len);
        Some(self.point(c, r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_no_overlap() {
        let mut p = Placer::new(Rect::new(0, 0, 42, 12), [0.0, 40.0], [0.0, 20.0]);
        let (x, y) = p.point(10, 5);
        assert_eq!(p.cell(x, y), (10, 5));
        let a = p.marker(x, y, 2).unwrap();
        let b = p.marker(x, y, 2).unwrap();
        assert_ne!(p.cell(a.0, a.1), p.cell(b.0, b.1));
        let l1 = p.label(x, y, 8).unwrap();
        let l2 = p.label(x, y, 8).unwrap();
        assert_ne!(p.cell(l1.0, l1.1), p.cell(l2.0, l2.1));
    }
}
