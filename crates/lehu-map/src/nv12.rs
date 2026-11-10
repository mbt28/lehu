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
