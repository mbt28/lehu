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
/// The part driven: grey, its edge darker.
const BEHIND: [u8; 3] = [160, 164, 172];
const BEHIND_EDGE: [u8; 3] = [112, 116, 124];
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

/// The route already driven: as `route`, in grey.
pub fn route_behind(
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
        BEHIND_EDGE,
    );
    thick(dst, pitch, chroma, size, clip, points, ROUTE_R, BEHIND);
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
    // Within r of the segment p-q: by the ends, or across the middle. In
    // i64 (no 128-bit library calls a pixel): the segments are cut to at
    // most `PIECE`, so no square overflows.
    let near = |p: (i64, i64), q: (i64, i64), c: (i64, i64)| {
        let (dx, dy) = (q.0 - p.0, q.1 - p.1);
        let (cx, cy) = (c.0 - p.0, c.1 - p.1);
        let r2 = r * r;
        let len2 = dx * dx + dy * dy;
        let dot = cx * dx + cy * dy;
        if dot <= 0 || len2 == 0 {
            return cx * cx + cy * cy <= r2;
        }
        if dot >= len2 {
            let (ex, ey) = (c.0 - q.0, c.1 - q.1);
            return ex * ex + ey * ey <= r2;
        }
        let cross = cx * dy - cy * dx;
        cross * cross <= r2 * len2
    };
    // The segments, the long ones cut (1/16 px: 256 pixels a piece).
    const PIECE: i64 = 256 * 16;
    let mut pieces = Vec::with_capacity(points.len());
    for pair in points.windows(2) {
        let (p, q) = (pair[0], pair[1]);
        // Wholly outside the clip: neither drawn nor cut.
        if ((p.0.max(q.0) + r) >> 4) < cl
            || ((p.0.min(q.0) - r) >> 4) >= cr
            || ((p.1.max(q.1) + r) >> 4) < ct
            || ((p.1.min(q.1) - r) >> 4) >= cb
        {
            continue;
        }
        let n = (q.0 - p.0).abs().max((q.1 - p.1).abs()) / PIECE + 1;
        for k in 0..n {
            let at = |k: i64| (p.0 + (q.0 - p.0) * k / n, p.1 + (q.1 - p.1) * k / n);
            pieces.push((at(k), at(k + 1)));
        }
    }
    for (p, q) in pieces {
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

/// Tiles laid out around a point for a rotated view: `cols` x `rows`
/// tiles (NV12, `side` a side), row by row; None where there is no tile.
pub struct Grid<'a> {
    pub tiles: Vec<Option<&'a [u8]>>,
    pub cols: usize,
    pub rows: usize,
    pub side: usize,
    /// The land colour, for outside the tiles: luma, Cb, Cr.
    pub land: (u8, u8, u8),
}

/// A heading-up frame: each output pixel (u, v) of a `width` x `height`
/// NV12 canvas samples the grid at
///   origin + (u - ax) * across + (v - ay) * down
/// where `origin` is the grid point at the anchor (ax, ay), and `across`
/// and `down` the grid steps of one output pixel right and down; all in
/// 1/65536 grid pixels. Nearest sample. Written 64 columns at a time, so
/// the grid's reads stay in the cache, and a row's bytes at once, so the
/// output (write-combined memory) is written in bursts.
///
/// The loop runs once a pixel, five frames a second, on a CPU without a
/// divide instruction: 32-bit fixed point (a grid of up to 128 tiles a
/// side fits), and the tiles found by shifting, never by dividing.
#[allow(clippy::too_many_arguments)]
pub fn rotate(
    grid: &Grid<'_>,
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    (width, height): (u32, u32),
    origin: (i64, i64),
    (ax, ay): (i64, i64),
    across: (i64, i64),
    down: (i64, i64),
) {
    const BLOCK: usize = 64;
    // Samples looked up together, from one tile when they lie in one.
    const SPAN: usize = 16;
    debug_assert_eq!(grid.side, 256, "the tiles are found by shifting");
    let (w, h) = (width as usize, height as usize);
    let (gw, gh) = ((grid.cols * 256) as u32, (grid.rows * 256) as u32);
    let cols = grid.cols;
    let mut row = [0u8; BLOCK];
    // Where (u, v) samples, in 1/65536 grid pixels; i32 from here on.
    let at = |u: i64, v: i64| {
        (
            (origin.0 + (u - ax) * across.0 + (v - ay) * down.0) as i32,
            (origin.1 + (u - ax) * across.1 + (v - ay) * down.1) as i32,
        )
    };
    let (ux, uy) = (across.0 as i32, across.1 as i32);
    // The luma at grid pixel (gx, gy), unsigned so one compare each way
    // also sends negatives to land.
    let luma = |gx: u32, gy: u32| -> u8 {
        if gx >= gw || gy >= gh {
            return grid.land.0;
        }
        match grid.tiles[(gy >> 8) as usize * cols + (gx >> 8) as usize] {
            Some(t) => t[(((gy & 255) << 8) | (gx & 255)) as usize],
            None => grid.land.0,
        }
    };
    let pair = |gx: u32, gy: u32| -> [u8; 2] {
        if gx >= gw || gy >= gh {
            return [grid.land.1, grid.land.2];
        }
        match grid.tiles[(gy >> 8) as usize * cols + (gx >> 8) as usize] {
            Some(t) => {
                let at = 65536 + (((gy & 255) >> 1) << 8) as usize + ((gx & 255) & !1) as usize;
                [t[at], t[at + 1]]
            }
            None => [grid.land.1, grid.land.2],
        }
    };
    // The tile a run of samples lies in, when the whole run does: its
    // ends in one tile put every sample between them there too (the run
    // is straight, the tile a rectangle). Some(None) for a missing tile.
    let tile_of = |(x0, y0): (i32, i32), (x1, y1): (i32, i32)| -> Option<Option<&[u8]>> {
        let (a, b) = (
            ((x0 >> 16) as u32, (y0 >> 16) as u32),
            ((x1 >> 16) as u32, (y1 >> 16) as u32),
        );
        if a.0 >= gw || a.1 >= gh || a.0 >> 8 != b.0 >> 8 || a.1 >> 8 != b.1 >> 8 {
            return None;
        }
        Some(grid.tiles[(a.1 >> 8) as usize * cols + (a.0 >> 8) as usize])
    };
    for c0 in (0..w).step_by(BLOCK) {
        let n = BLOCK.min(w - c0);
        for v in 0..h {
            let (mut x, mut y) = at(c0 as i64, v as i64);
            for run in row[..n].chunks_mut(SPAN) {
                let last = (run.len() - 1) as i32;
                let end = (x.wrapping_add(ux * last), y.wrapping_add(uy * last));
                match tile_of((x, y), end) {
                    Some(Some(t)) => {
                        let t: &[u8; 65536] = t[..65536].try_into().expect("a tile's luma");
                        for o in run.iter_mut() {
                            let i =
                                (((y >> 8) as u32 & 0xff00) | ((x >> 16) as u32 & 0xff)) as usize;
                            *o = t[i];
                            x = x.wrapping_add(ux);
                            y = y.wrapping_add(uy);
                        }
                        continue;
                    }
                    Some(None) => run.fill(grid.land.0),
                    None => {
                        for o in run.iter_mut() {
                            *o = luma((x >> 16) as u32, (y >> 16) as u32);
                            x = x.wrapping_add(ux);
                            y = y.wrapping_add(uy);
                        }
                        continue;
                    }
                }
                x = x.wrapping_add(ux * run.len() as i32);
                y = y.wrapping_add(uy * run.len() as i32);
            }
            let o = v * pitch + c0;
            if let Some(out) = dst.get_mut(o..o + n) {
                out.copy_from_slice(&row[..n]);
            }
        }
        // The chroma: a pair for each two by two block, sampled at the
        // block's middle; the pairs of a 64-pixel block are 64 bytes.
        for cv in 0..h / 2 {
            let (mut x, mut y) = at(c0 as i64, 2 * cv as i64);
            // To the block's middle: half a pixel right and down.
            x = x.wrapping_add(((across.0 + down.0) / 2) as i32);
            y = y.wrapping_add(((across.1 + down.1) / 2) as i32);
            for run in row[..n & !1].as_chunks_mut::<2>().0.chunks_mut(SPAN / 2) {
                let last = (run.len() - 1) as i32;
                let end = (x.wrapping_add(2 * ux * last), y.wrapping_add(2 * uy * last));
                match tile_of((x, y), end) {
                    Some(Some(t)) => {
                        // The tile's pairs: 128 rows of 128.
                        let pairs: &[[u8; 2]; 16384] = t[65536..98304]
                            .as_chunks::<2>()
                            .0
                            .try_into()
                            .expect("a tile's chroma");
                        for p in run.iter_mut() {
                            let i =
                                (((y >> 10) as u32 & 0x3f80) | ((x >> 17) as u32 & 0x7f)) as usize;
                            *p = pairs[i];
                            x = x.wrapping_add(2 * ux);
                            y = y.wrapping_add(2 * uy);
                        }
                        continue;
                    }
                    Some(None) => run.fill([grid.land.1, grid.land.2]),
                    None => {
                        for p in run.iter_mut() {
                            *p = pair((x >> 16) as u32, (y >> 16) as u32);
                            x = x.wrapping_add(2 * ux);
                            y = y.wrapping_add(2 * uy);
                        }
                        continue;
                    }
                }
                x = x.wrapping_add(2 * ux * run.len() as i32);
                y = y.wrapping_add(2 * uy * run.len() as i32);
            }
            let o = chroma + cv * pitch + c0;
            if let Some(out) = dst.get_mut(o..o + (n & !1)) {
                out.copy_from_slice(&row[..n & !1]);
            }
        }
    }
}

/// An NV12 picture to draw on: its bytes, its rows `pitch` bytes apart,
/// its chroma from byte `chroma`, `size` pixels.
pub struct Surface<'a> {
    pub pixels: &'a mut [u8],
    pub pitch: usize,
    pub chroma: usize,
    pub size: (u32, u32),
}

/// A label's picture, upright: `w` x `h` bytes, row by row, 0 where it is
/// clear, else an index into the lumas it is stamped with (`label_lumas`);
/// `anchor` is the point stamped at the label's place, in 1/16 pixels.
pub struct Stamp<'a> {
    pub w: usize,
    pub h: usize,
    pub px: &'a [u8],
    pub anchor: (i64, i64),
}

/// The lumas a label is stamped with: index 1 is the halo, 255 the text,
/// those between the text's edge over the halo.
pub fn label_lumas(text: [u8; 3], halo: [u8; 3]) -> [u8; 256] {
    let (t, h) = (i32::from(ycc(text).0), i32::from(ycc(halo).0));
    let mut lut = [0u8; 256];
    for (i, o) in lut.iter_mut().enumerate().skip(1) {
        let cover = (i as i32 - 1) * 255 / 254;
        *o = ((h * (255 - cover) + t * cover + 127) / 255) as u8;
    }
    lut
}

/// Stamp `s` on `out`, only inside `clip` (left, top, right, bottom, in
/// pixels), its anchor at `at` (1/16 pixels), its rows running along `way`
/// (65536 long; x right, y down): its luma through `lumas`, and the
/// chroma pair `pair` for each two by two block whose middle it covers.
/// Nearest sample, in integers: each output pixel samples the stamp at
///   anchor + ((p - at) . way, (p - at) . way turned a quarter clockwise)
/// in 16.16 fixed point, a row's samples one add apart; only the columns
/// of a row that the stamp can cover are visited, found with a reciprocal
/// worked out once (the CPU has no divide). Written, never read.
pub fn stamp(
    out: &mut Surface<'_>,
    clip: (i64, i64, i64, i64),
    s: &Stamp<'_>,
    at: (i64, i64),
    way: (i64, i64),
    lumas: &[u8; 256],
    pair: [u8; 2],
) {
    let (w, h) = (i64::from(out.size.0), i64::from(out.size.1));
    let (cl, ct) = (clip.0.max(0), clip.1.max(0));
    let (cr, cb) = (clip.2.min(w), clip.3.min(h));
    if cl >= cr || ct >= cb || s.w == 0 || s.h == 0 || s.px.len() < s.w * s.h {
        return;
    }
    let (c, sn) = way;
    if c == 0 && sn == 0 {
        return;
    }
    // The stamp's corners on the picture (1/16 px), for the rows it spans.
    let corner = |u: i64, v: i64| {
        let (u, v) = (u * 16 - s.anchor.0, v * 16 - s.anchor.1);
        (
            at.0 + ((u * c - v * sn) >> 16),
            at.1 + ((u * sn + v * c) >> 16),
        )
    };
    let (sw, sh) = (s.w as i64, s.h as i64);
    let corners = [corner(0, 0), corner(sw, 0), corner(0, sh), corner(sw, sh)];
    let xs = corners.iter().map(|p| p.0);
    let ys = corners.iter().map(|p| p.1);
    let x0 = ((xs.clone().min().unwrap_or(0) >> 4) - 1).max(cl);
    let x1 = ((xs.max().unwrap_or(0) >> 4) + 2).min(cr);
    let y0 = ((ys.clone().min().unwrap_or(0) >> 4) - 1).max(ct);
    let y1 = ((ys.max().unwrap_or(0) >> 4) + 2).min(cb);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let (lim_u, lim_v) = (sw << 16, sh << 16);
    let (a0, a1) = (s.anchor.0 << 12, s.anchor.1 << 12);
    let rec = |k: i64| if k == 0 { 0 } else { (1i64 << 32) / k.abs() };
    let (rec_c, rec_s) = (rec(c), rec(sn));
    let (rec_c2, rec_s2) = (rec(2 * c), rec(2 * sn));
    let (ci, si) = (c as i32, sn as i32);
    let (wu, hu) = (lim_u as u32, lim_v as u32);
    let sample = |u: i32, v: i32| -> u8 {
        if (u as u32) < wu && (v as u32) < hu {
            s.px[(v >> 16) as usize * s.w + (u >> 16) as usize]
        } else {
            0
        }
    };
    let (pitch, chroma) = (out.pitch, out.chroma);
    for y in y0..y1 {
        // Where column 0 of this row samples, in 16.16.
        let dy = y * 16 + 8 - at.1;
        let dx = 8 - at.0;
        let ub = ((dx * c + dy * sn) >> 4) + a0;
        let vb = ((dy * c - dx * sn) >> 4) + a1;
        let (a, b) = columns(ub, c, rec_c, lim_u, (x0, x1));
        let (a, b) = columns(vb, -sn, rec_s, lim_v, (a, b));
        if a >= b {
            continue;
        }
        let o = y as usize * pitch;
        let Some(line) = out.pixels.get_mut(o + a as usize..o + b as usize) else {
            continue;
        };
        let mut u = (ub + a * c) as i32;
        let mut v = (vb - a * sn) as i32;
        for p in line {
            let i = sample(u, v);
            if i != 0 {
                *p = lumas[usize::from(i)];
            }
            u = u.wrapping_add(ci);
            v = v.wrapping_sub(si);
        }
    }
    // The chroma: a pair a block, sampled at the block's middle.
    let (bx0, bx1) = (x0 / 2, (x1 + 1) / 2);
    for by in (y0 / 2)..((y1 + 1) / 2) {
        let dy = by * 32 + 16 - at.1;
        let dx = 16 - at.0;
        let ub = ((dx * c + dy * sn) >> 4) + a0;
        let vb = ((dy * c - dx * sn) >> 4) + a1;
        let (a, b) = columns(ub, 2 * c, rec_c2, lim_u, (bx0, bx1));
        let (a, b) = columns(vb, -2 * sn, rec_s2, lim_v, (a, b));
        if a >= b {
            continue;
        }
        let o = chroma + by as usize * pitch;
        let Some(line) = out.pixels.get_mut(o + 2 * a as usize..o + 2 * b as usize) else {
            continue;
        };
        let mut u = (ub + a * 2 * c) as i32;
        let mut v = (vb - a * 2 * sn) as i32;
        for p in line.as_chunks_mut::<2>().0 {
            if sample(u, v) != 0 {
                *p = pair;
            }
            u = u.wrapping_add(2 * ci);
            v = v.wrapping_sub(2 * si);
        }
    }
}

/// The columns `x` of `range` where `0 <= a + x * k < limit` can hold: the
/// exact ones and one more each side, from `rec` (2^32 / |k|), by
/// multiplying. All of `range` when `k` is 0 and `a` is inside, else none.
fn columns(a: i64, k: i64, rec: i64, limit: i64, (x0, x1): (i64, i64)) -> (i64, i64) {
    if k == 0 {
        return if (0..limit).contains(&a) {
            (x0, x1)
        } else {
            (x0, x0)
        };
    }
    let p = (-a * rec) >> 32;
    let q = ((limit - a) * rec) >> 32;
    let (lo, hi) = if k > 0 { (p, q) } else { (-q, -p) };
    ((lo - 1).max(x0), (hi + 2).min(x1))
}
