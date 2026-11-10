// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map's pixels: NV12, limited range, BT.601, which the display's
//! colour conversion turns back into RGB. A tile is its luma rows, then
//! its chroma rows (Cb, Cr pairs, half as many).

/// JPEG's full-range luma (0..255) to the limited range (16..235).
pub const FULL_TO_LIMITED_Y: [u8; 256] = range(219, 16);

/// JPEG's full-range chroma (0..255 about 128) to 16..240.
pub const FULL_TO_LIMITED_C: [u8; 256] = range(224, 16);

const fn range(span: u32, low: u32) -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut v = 0;
    while v < 256 {
        t[v] = ((v as u32 * span + low * 255 + 127) / 255) as u8;
        v += 1;
    }
    t
}

/// An RGB colour's limited-range luma and chroma.
pub fn ycc([r, g, b]: [u8; 3]) -> (u8, u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let y = 16 + ((66 * r + 129 * g + 25 * b + 128) >> 8);
    let cb = 128 + ((-38 * r - 74 * g + 112 * b + 128) >> 8);
    let cr = 128 + ((112 * r - 94 * g - 18 * b + 128) >> 8);
    (
        y.clamp(16, 235) as u8,
        cb.clamp(16, 240) as u8,
        cr.clamp(16, 240) as u8,
    )
}

/// A `side` x `side` RGB picture (three bytes a pixel) as an NV12 tile;
/// each chroma sample from the mean of its two by two pixels.
pub fn from_rgb(rgb: &[u8], side: usize, dst: &mut [u8]) {
    let (dst_y, dst_c) = dst.split_at_mut(side * side);
    let (px, _) = rgb.as_chunks::<3>();
    for (o, &p) in dst_y.iter_mut().zip(px) {
        *o = ycc(p).0;
    }
    for cy in 0..side / 2 {
        for cx in 0..side / 2 {
            let mut sum = [0u32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = px[(2 * cy + dy) * side + 2 * cx + dx];
                for k in 0..3 {
                    sum[k] += u32::from(p[k]);
                }
            }
            let mean = sum.map(|s| ((s + 2) / 4) as u8);
            let (_, cb, cr) = ycc(mean);
            let at = cy * side + 2 * cx;
            dst_c[at] = cb;
            dst_c[at + 1] = cr;
        }
    }
}

/// The position dot: its fill and ring, and their radii in pixels.
const DOT: [u8; 3] = [26, 115, 232];
const RING: [u8; 3] = [255, 255, 255];
const DOT_R: i64 = 9;
const RING_R: i64 = 13;

/// Draw the position dot centred at (`cx`, `cy`) on an NV12 canvas of
/// `width` x `height` whose rows are `pitch` bytes apart and whose chroma
/// begins at `chroma`; the parts off the canvas are left out. Written, not
/// read: the canvas may be write-combined memory.
pub fn dot(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    (width, height): (u32, u32),
    (cx, cy): (i64, i64),
) {
    let (w, h) = (i64::from(width), i64::from(height));
    if cx + RING_R < 0 || cy + RING_R < 0 || cx - RING_R >= w || cy - RING_R >= h {
        return;
    }
    let (dot_y, dot_cb, dot_cr) = ycc(DOT);
    let (ring_y, ring_cb, ring_cr) = ycc(RING);
    let inside = |dx: i64, dy: i64, r: i64| dx * dx + dy * dy <= r * r;
    for y in (cy - RING_R).max(0)..(cy + RING_R + 1).min(h) {
        for x in (cx - RING_R).max(0)..(cx + RING_R + 1).min(w) {
            let (dx, dy) = (x - cx, y - cy);
            let v = if inside(dx, dy, DOT_R) {
                dot_y
            } else if inside(dx, dy, RING_R) {
                ring_y
            } else {
                continue;
            };
            if let Some(p) = dst.get_mut(y as usize * pitch + x as usize) {
                *p = v;
            }
        }
    }
    // The chroma: one pair a two by two block, by the block's middle.
    for by in ((cy - RING_R).max(0) / 2)..((cy + RING_R + 2).min(h) / 2) {
        for bx in ((cx - RING_R).max(0) / 2)..((cx + RING_R + 2).min(w) / 2) {
            // The block's middle, in half pixels against the centre's.
            let (dx2, dy2) = (4 * bx + 1 - 2 * cx, 4 * by + 1 - 2 * cy);
            let (cb, cr) = if inside(dx2, dy2, 2 * DOT_R) {
                (dot_cb, dot_cr)
            } else if inside(dx2, dy2, 2 * RING_R) {
                (ring_cb, ring_cr)
            } else {
                continue;
            };
            let at = chroma + by as usize * pitch + 2 * bx as usize;
            if let Some(p) = dst.get_mut(at..at + 2) {
                p.copy_from_slice(&[cb, cr]);
            }
        }
    }
}
