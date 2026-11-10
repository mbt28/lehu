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

/// The quarter (`qx`, `qy`, each 0 or 1) of an NV12 tile `side` pixels a
/// side, at twice the size, as a whole tile: bilinear, from the samples
/// around it in the tile too, so the quarters meet without a seam.
pub fn enlarge_quarter(src: &[u8], side: usize, (qx, qy): (usize, usize), dst: &mut [u8]) {
    let (src_y, src_c) = src.split_at(side * side);
    let (dst_y, dst_c) = dst.split_at_mut(side * side);
    let half = side / 2;
    up2(
        src_y,
        (side, side),
        1,
        (qx * half, qy * half),
        (half, half),
        dst_y,
    );
    // The chroma: half as many pairs each way, each of the pair alone.
    let quarter = side / 4;
    up2(
        src_c,
        (half, half),
        2,
        (qx * quarter, qy * quarter),
        (quarter, quarter),
        dst_c,
    );
}

/// Twice the size, bilinear: the `w` x `h` region at (`ox`, `oy`) of a
/// plane of `sw` x `sh` samples, `n` bytes to a sample (each byte on its
/// own), into `dst`, `2w` x `2h` samples. Each new sample is 3/4 of its
/// nearest and 1/4 of the next each way; at the plane's edge the nearest
/// stands in for the next. Done a row at a time: the two source rows
/// blended once (3:1), then the blend widened (3:1 again).
fn up2(
    src: &[u8],
    (sw, sh): (usize, usize),
    n: usize,
    (ox, oy): (usize, usize),
    (w, h): (usize, usize),
    dst: &mut [u8],
) {
    // The region's columns and one more each side (the plane's edge
    // repeated where there is none).
    let x0 = ox.saturating_sub(1);
    let x1 = (ox + w + 1).min(sw);
    let pad_left = usize::from(ox == 0);
    let mut blend = vec![0u16; (w + 2) * n];
    let row = 2 * w * n;
    for dy in 0..2 * h {
        let sy = oy + dy / 2;
        let ny = if dy % 2 == 0 {
            sy.saturating_sub(1)
        } else {
            (sy + 1).min(sh - 1)
        };
        let a = &src[(sy * sw + x0) * n..(sy * sw + x1) * n];
        let b = &src[(ny * sw + x0) * n..(ny * sw + x1) * n];
        // blend[k] is source column ox - 1 + k, in sixteenths / 4.
        let at = pad_left * n;
        for ((o, &p), &q) in blend[at..].iter_mut().zip(a).zip(b) {
            *o = 3 * u16::from(p) + u16::from(q);
        }
        if pad_left == 1 {
            blend.copy_within(n..2 * n, 0);
        }
        let filled = at + a.len();
        if filled < blend.len() {
            blend.copy_within(filled - n..filled, filled);
        }
        let line = &mut dst[dy * row..(dy + 1) * row];
        for i in 0..w {
            let (l, m, r) = (i * n, (i + 1) * n, (i + 2) * n);
            for c in 0..n {
                let mid = 3 * u32::from(blend[m + c]);
                line[2 * i * n + c] = ((mid + u32::from(blend[l + c]) + 8) / 16) as u8;
                line[(2 * i + 1) * n + c] = ((mid + u32::from(blend[r + c]) + 8) / 16) as u8;
            }
        }
    }
}

/// The heading arrow: half its length, in pixels, and its white edge.
const ARROW_R: f64 = 15.0;
const ARROW_EDGE: f64 = 3.0;

/// How far from its middle the dot or the arrow can reach, in pixels, with
/// a pixel to spare.
pub const MARKER_REACH: i64 = 23;

/// Draw the heading arrow centred at (`cx`, `cy`), pointing along
/// `heading` (any length, x right, y down), on an NV12 canvas as `dot`
/// does: a blue arrowhead with a white edge.
pub fn arrow(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    (cx, cy): (i64, i64),
    heading: (i32, i32),
) {
    let (hx, hy) = (f64::from(heading.0), f64::from(heading.1));
    let len = hx.hypot(hy);
    if len == 0.0 {
        dot(dst, pitch, chroma, size, (cx, cy));
        return;
    }
    let (ux, uy) = (hx / len, hy / len);
    // An arrowhead: the tip ahead, two wings behind, a notch between them;
    // as two triangles, tip-wing-notch on each side. Corners in 1/16 px.
    let shape = |r: f64| {
        let at = |along: f64, across: f64| {
            let x = cx as f64 + ux * along - uy * across;
            let y = cy as f64 + uy * along + ux * across;
            ((x * 16.0) as i64, (y * 16.0) as i64)
        };
        let tip = at(r, 0.0);
        let left = at(-0.8 * r, -0.75 * r);
        let right = at(-0.8 * r, 0.75 * r);
        let notch = at(-0.35 * r, 0.0);
        [[tip, left, notch], [tip, notch, right]]
    };
    fill(
        dst,
        pitch,
        chroma,
        size,
        &shape(ARROW_R + ARROW_EDGE * 1.6),
        RING,
    );
    fill(dst, pitch, chroma, size, &shape(ARROW_R), DOT);
}

/// Fill the triangles (corners in 1/16 px) with `colour`, clipped to the
/// canvas: each pixel by its middle, each chroma pair by its block's.
fn fill(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    (width, height): (u32, u32),
    tris: &[[(i64, i64); 3]],
    colour: [u8; 3],
) {
    let (w, h) = (i64::from(width), i64::from(height));
    let (y_val, cb, cr) = ycc(colour);
    let xs = tris.iter().flatten().map(|p| p.0);
    let ys = tris.iter().flatten().map(|p| p.1);
    let (x0, x1) = (
        xs.clone().min().unwrap_or(0) / 16 - 1,
        xs.max().unwrap_or(0) / 16 + 1,
    );
    let (y0, y1) = (
        ys.clone().min().unwrap_or(0) / 16 - 1,
        ys.max().unwrap_or(0) / 16 + 1,
    );
    if x1 < 0 || y1 < 0 || x0 >= w || y0 >= h {
        return;
    }
    let edge = |a: (i64, i64), b: (i64, i64), p: (i64, i64)| {
        (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0)
    };
    let inside = |p: (i64, i64)| {
        tris.iter().any(|t| {
            let (e0, e1, e2) = (
                edge(t[0], t[1], p),
                edge(t[1], t[2], p),
                edge(t[2], t[0], p),
            );
            (e0 >= 0 && e1 >= 0 && e2 >= 0) || (e0 <= 0 && e1 <= 0 && e2 <= 0)
        })
    };
    for y in y0.max(0)..=y1.min(h - 1) {
        for x in x0.max(0)..=x1.min(w - 1) {
            if inside((x * 16 + 8, y * 16 + 8))
                && let Some(p) = dst.get_mut(y as usize * pitch + x as usize)
            {
                *p = y_val;
            }
        }
    }
    for by in (y0.max(0) / 2)..=(y1.min(h - 1) / 2) {
        for bx in (x0.max(0) / 2)..=(x1.min(w - 1) / 2) {
            // The block's middle: between its four pixels.
            if inside((bx * 32 + 16, by * 32 + 16)) {
                let at = chroma + by as usize * pitch + 2 * bx as usize;
                if let Some(p) = dst.get_mut(at..at + 2) {
                    p.copy_from_slice(&[cb, cr]);
                }
            }
        }
    }
}

/// The route line: its fill and edge, and their half widths in 1/16 px.
const ROUTE: [u8; 3] = [66, 133, 244];
const ROUTE_EDGE: [u8; 3] = [24, 82, 190];
const ROUTE_R: i64 = 4 * 16;
const ROUTE_EDGE_R: i64 = 6 * 16;

/// Draw the route through `points` (canvas pixels times 16) on an NV12
/// canvas, only inside `clip` (left, top, right, bottom, in pixels): a
/// line with a darker edge, round where it turns.
pub fn route(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    clip: (i64, i64, i64, i64),
    points: &[(i64, i64)],
) {
    thick(
        dst,
        pitch,
        chroma,
        size,
        clip,
        points,
        ROUTE_EDGE_R,
        ROUTE_EDGE,
    );
    thick(dst, pitch, chroma, size, clip, points, ROUTE_R, ROUTE);
}

/// Every pixel (by its middle) and chroma pair (by its block's) within
/// `r` of the line through `points`, inside `clip`, set to `colour`.
#[allow(clippy::too_many_arguments)]
fn thick(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    (width, height): (u32, u32),
    clip: (i64, i64, i64, i64),
    points: &[(i64, i64)],
    r: i64,
    colour: [u8; 3],
) {
    let (w, h) = (i64::from(width), i64::from(height));
    let (cl, ct) = (clip.0.max(0), clip.1.max(0));
    let (cr, cb) = (clip.2.min(w), clip.3.min(h));
    if cl >= cr || ct >= cb {
        return;
    }
    let (y_val, cb_val, cr_val) = ycc(colour);
    // Within r of the segment p-q: by the ends, or across the middle.
    let near = |p: (i64, i64), q: (i64, i64), c: (i64, i64)| {
        let (dx, dy) = (i128::from(q.0 - p.0), i128::from(q.1 - p.1));
        let (cx, cy) = (i128::from(c.0 - p.0), i128::from(c.1 - p.1));
        let r2 = i128::from(r) * i128::from(r);
        let len2 = dx * dx + dy * dy;
        let dot = cx * dx + cy * dy;
        if dot <= 0 || len2 == 0 {
            return cx * cx + cy * cy <= r2;
        }
        if dot >= len2 {
            let (ex, ey) = (i128::from(c.0 - q.0), i128::from(c.1 - q.1));
            return ex * ex + ey * ey <= r2;
        }
        let cross = cx * dy - cy * dx;
        cross * cross <= r2 * len2
    };
    for pair in points.windows(2) {
        let (p, q) = (pair[0], pair[1]);
        // The segment's box, widened by r, in pixels, inside the clip.
        let x0 = ((p.0.min(q.0) - r) >> 4).max(cl);
        let x1 = (((p.0.max(q.0) + r) >> 4) + 1).min(cr);
        let y0 = ((p.1.min(q.1) - r) >> 4).max(ct);
        let y1 = (((p.1.max(q.1) + r) >> 4) + 1).min(cb);
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                if near(p, q, (x * 16 + 8, y * 16 + 8))
                    && let Some(v) = dst.get_mut(y as usize * pitch + x as usize)
                {
                    *v = y_val;
                }
            }
        }
        for by in (y0 / 2)..((y1 + 1) / 2) {
            for bx in (x0 / 2)..((x1 + 1) / 2) {
                if near(p, q, (bx * 32 + 16, by * 32 + 16)) {
                    let at = chroma + by as usize * pitch + 2 * bx as usize;
                    if let Some(v) = dst.get_mut(at..at + 2) {
                        v.copy_from_slice(&[cb_val, cr_val]);
                    }
                }
            }
        }
    }
}
