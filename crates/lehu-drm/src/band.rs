// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Rectangles on the panel and the window the overlay plane covers. Pure
//! arithmetic, so it is tested without a device.

/// `[x0, x1) x [y0, y1)` in panel pixels, inside the panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

impl Rect {
    pub fn width(self) -> u32 {
        self.x1 - self.x0
    }

    pub fn height(self) -> u32 {
        self.y1 - self.y0
    }
}

/// `[x0, x1) x [y0, y1)` clipped to a `width` x `height` panel; None when
/// nothing is left. Coordinates off the panel are clipped, not refused: a
/// band is "the rows with content", and content at the edge is content.
pub fn clip(x0: i64, y0: i64, x1: i64, y1: i64, width: u32, height: u32) -> Option<Rect> {
    let x0 = x0.max(0);
    let y0 = y0.max(0);
    let x1 = x1.min(i64::from(width));
    let y1 = y1.min(i64::from(height));
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    // All four lie in 0..=u32::MAX after the clamps above.
    Some(Rect {
        x0: x0 as u32,
        y0: y0 as u32,
        x1: x1 as u32,
        y1: y1 as u32,
    })
}

/// Whether `w` x `h` pixels at (`x`, `y`) lie inside a `width` x `height`
/// buffer, with no empty side.
pub fn inside(x: u32, y: u32, w: u32, h: u32, width: u32, height: u32) -> bool {
    w > 0 && h > 0 && x < width && y < height && w <= width - x && h <= height - y
}

/// The bytes `rows` rows of `row_bytes` take when they start `pitch` bytes
/// apart: the last row needs only its pixels. None when it overflows.
pub fn span(rows: usize, row_bytes: usize, pitch: usize) -> Option<usize> {
    if rows == 0 || row_bytes == 0 || pitch < row_bytes {
        return None;
    }
    (rows - 1).checked_mul(pitch)?.checked_add(row_bytes)
}

/// The window the overlay plane covers, or nothing while it is disabled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Band {
    shown: Option<Rect>,
}

impl Band {
    pub fn is_visible(self) -> bool {
        self.shown.is_some()
    }

    /// A commit for `r` would change nothing: the plane shows exactly this.
    pub fn same(self, r: Rect) -> bool {
        self.shown == Some(r)
    }

    pub fn set(&mut self, r: Rect) {
        self.shown = Some(r);
    }

    pub fn clear(&mut self) {
        self.shown = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 800;
    const H: u32 = 480;

    fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }

    #[test]
    fn clip_keeps_what_is_on_the_panel() {
        assert_eq!(clip(0, 0, 800, 480, W, H), Some(rect(0, 0, 800, 480)));
        assert_eq!(clip(10, 20, 30, 40, W, H), Some(rect(10, 20, 30, 40)));
        // Beyond the edges: clipped to them.
        assert_eq!(clip(-5, -5, 900, 500, W, H), Some(rect(0, 0, 800, 480)));
        assert_eq!(
            clip(700, 400, 1000, 1000, W, H),
            Some(rect(700, 400, 800, 480))
        );
        assert_eq!(rect(700, 400, 800, 480).width(), 100);
        assert_eq!(rect(700, 400, 800, 480).height(), 80);
    }

    #[test]
    fn clip_refuses_what_is_empty_or_off_the_panel() {
        assert_eq!(clip(0, 100, 800, 100, W, H), None);
        assert_eq!(clip(0, 200, 800, 100, W, H), None);
        assert_eq!(clip(800, 0, 900, 480, W, H), None);
        assert_eq!(clip(0, 480, 800, 500, W, H), None);
        assert_eq!(clip(-100, 0, -1, 480, W, H), None);
        assert_eq!(clip(0, 0, i64::MAX, i64::MIN, W, H), None);
    }

    #[test]
    fn inside_checks_every_side() {
        assert!(inside(0, 0, W, H, W, H));
        assert!(inside(799, 479, 1, 1, W, H));
        assert!(!inside(0, 0, 0, 1, W, H));
        assert!(!inside(0, 0, 1, 0, W, H));
        assert!(!inside(800, 0, 1, 1, W, H));
        assert!(!inside(0, 480, 1, 1, W, H));
        assert!(!inside(1, 0, W, 1, W, H));
        assert!(!inside(0, 1, 1, H, W, H));
        // Large values never wrap around.
        assert!(!inside(1, 1, u32::MAX, u32::MAX, W, H));
    }

    #[test]
    fn span_counts_the_last_row_short() {
        assert_eq!(span(1, 1600, 1600), Some(1600));
        assert_eq!(span(3, 1600, 1600), Some(4800));
        assert_eq!(span(3, 1600, 2000), Some(5600));
        assert_eq!(span(0, 1600, 1600), None);
        assert_eq!(span(3, 0, 1600), None);
        // Rows closer than their own length overlap: refused.
        assert_eq!(span(3, 1600, 1000), None);
        assert_eq!(span(usize::MAX, 16, usize::MAX), None);
    }

    #[test]
    fn band_tells_a_repeat_from_a_change() {
        let mut band = Band::default();
        let home = rect(0, 0, W, H);
        let toast = rect(0, 400, W, H);
        assert!(!band.is_visible());
        // Nothing shown yet: even the same rectangle is a change.
        assert!(!band.same(home));
        band.set(home);
        assert!(band.is_visible());
        assert!(band.same(home));
        assert!(!band.same(toast));
        band.set(toast);
        assert!(band.same(toast));
        band.clear();
        assert!(!band.is_visible());
        assert!(!band.same(toast));
    }
}
