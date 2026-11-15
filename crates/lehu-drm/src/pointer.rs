// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The mouse pointer's picture: the familiar arrow, white with a black
//! edge, in a 32 pixel square for the cursor plane. Premultiplied, in at
//! most 14 colours, so a cursor with a 16-colour palette (the F1C200s's)
//! shows it exactly.

/// The square's side, and where in it the arrow's tip is: the place the
/// pointer points at.
pub(crate) const SIZE: u32 = 32;
pub(crate) const HOT: (i32, i32) = (2, 2);

/// The arrow's outline, the tip first, in pixels from the tip.
const SHAPE: [(f32, f32); 7] = [
    (0.0, 0.0),
    (0.0, 21.0),
    (5.2, 16.5),
    (9.0, 25.0),
    (12.5, 23.5),
    (8.9, 15.4),
    (15.4, 15.4),
];
/// The black edge's width, in pixels outside the white.
const EDGE: f32 = 1.4;

/// Draw the pointer into `dst`, B G R A rows `pitch` bytes apart, its
/// square's top left at the start.
pub(crate) fn draw(dst: &mut [u8], pitch: usize) {
    let size = SIZE as usize;
    for y in 0..size {
        for x in 0..size {
            let (mut out, mut fill) = (0u32, 0u32);
            for s in 0..16 {
                let px = x as f32 + 0.125 + 0.25 * (s % 4) as f32 - HOT.0 as f32;
                let py = y as f32 + 0.125 + 0.25 * (s / 4) as f32 - HOT.1 as f32;
                if inside(px, py) {
                    fill += 1;
                    out += 1;
                } else if distance(px, py) <= EDGE {
                    out += 1;
                }
            }
            let at = y * pitch + x * 4;
            if let Some(p) = dst.get_mut(at..at + 4) {
                p.copy_from_slice(&pixel(out, fill));
            }
        }
    }
}

/// A pixel from `out` of 16 samples covered, `fill` of them white: alpha
/// in quarters, the white's share in quarters where it is opaque and in
/// halves at the rim. Premultiplied, B G R A.
fn pixel(out: u32, fill: u32) -> [u8; 4] {
    let a = (out * 4 + 8) / 16;
    if a == 0 {
        return [0; 4];
    }
    let steps = if a == 4 { 4 } else { 2 };
    let k = (fill * steps + out / 2) / out;
    let alpha = a * 255 / 4;
    let c = ((255 * k / steps) * alpha / 255) as u8;
    [c, c, c, alpha as u8]
}

/// Even-odd: (x, y) is inside the outline.
fn inside(x: f32, y: f32) -> bool {
    let mut odd = false;
    let n = SHAPE.len();
    for i in 0..n {
        let (a, b) = (SHAPE[i], SHAPE[(i + 1) % n]);
        if (a.1 > y) != (b.1 > y) && x < a.0 + (y - a.1) * (b.0 - a.0) / (b.1 - a.1) {
            odd = !odd;
        }
    }
    odd
}

/// How far (x, y) is from the outline.
fn distance(x: f32, y: f32) -> f32 {
    let n = SHAPE.len();
    (0..n)
        .map(|i| {
            let (a, b) = (SHAPE[i], SHAPE[(i + 1) % n]);
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let t = (((x - a.0) * dx + (y - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
            let (ex, ey) = (a.0 + t * dx - x, a.1 + t * dy - y);
            (ex * ex + ey * ey).sqrt()
        })
        .fold(f32::MAX, f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pointer_fits_its_square_and_a_16_colour_palette() {
        let size = SIZE as usize;
        let mut px = vec![0u8; size * size * 4];
        draw(&mut px, size * 4);
        let mut colours = std::collections::BTreeSet::new();
        let chunks = px.as_chunks::<4>().0;
        for p in chunks {
            assert!(p[..3].iter().all(|&c| c <= p[3]), "premultiplied: {p:?}");
            if p[3] != 0 {
                colours.insert(*p);
            }
        }
        assert!(colours.len() <= 15, "{} colours", colours.len());
        // White inside, a black edge, nothing at the far corner.
        let at = |x: usize, y: usize| chunks[y * size + x];
        assert_eq!(at(4, 12), [255, 255, 255, 255]);
        assert_eq!(at(HOT.0 as usize - 1, 12)[..3], [0, 0, 0]);
        assert_eq!(at(size - 1, 0), [0; 4]);
        // The shape and its edge stay inside the square.
        for &(x, y) in &SHAPE {
            assert!(x + HOT.0 as f32 + EDGE < SIZE as f32 && y + HOT.1 as f32 + EDGE < SIZE as f32);
        }
    }
}
