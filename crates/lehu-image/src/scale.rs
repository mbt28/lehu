// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Rows to the shown size: an area-weighted box filter (each pixel out
//! the mean of the input it covers, a pixel cut by its edge counted by the
//! part inside) fed one decoded row at a time, then a bilinear stretch for
//! the rare picture smaller than its box. Two output rows are gathered at
//! a time; the picture at its shown size is all it keeps.

use crate::{Error, Picture, fit};

pub(crate) struct Scaler {
    width: usize,
    height: usize,
    /// The original picture's size, of which each input pixel is a `run`
    /// x `run` square (a JPEG shrunk by its DCT; 1 otherwise).
    run: usize,
    orig_w: usize,
    orig_h: usize,
    /// The box-filtered size: the shown size, never more than the input.
    sw: usize,
    sh: usize,
    /// The shown size.
    tw: usize,
    th: usize,
    /// Each input column's first output column and its weights there and
    /// in the next (of `orig_w` an output column).
    xmap: Vec<(u16, u16, u16)>,
    /// The input row filtered across, in 256ths of a level.
    across: Vec<u32>,
    /// The output row being gathered and the one after it: input rows
    /// across, weighted by their part in it (of `orig_h`).
    cur: Vec<u32>,
    next: Vec<u32>,
    /// The output row `cur` is.
    oy: usize,
    /// Input rows taken.
    y: usize,
    out: Vec<u8>,
}

impl Scaler {
    /// For a `width` x `height` picture (checked by the caller against the
    /// size bounds) shown in a `side` box.
    pub(crate) fn new(width: u32, height: u32, side: u32) -> Self {
        Self::shrunk(width, height, 1, width, height, side)
    }

    /// For a `width` x `height` picture that is an `orig_w` x `orig_h` one
    /// already shrunk `run` times, its last row and column partly outside
    /// the original. The shown size is of the original, and `run` x the
    /// shown size must not pass it: an input pixel then falls in at most
    /// two output pixels each way.
    pub(crate) fn shrunk(
        width: u32,
        height: u32,
        run: u32,
        orig_w: u32,
        orig_h: u32,
        side: u32,
    ) -> Self {
        let (tw, th) = fit(orig_w, orig_h, side);
        let (width, height, run) = (width as usize, height as usize, run as usize);
        let (orig_w, orig_h) = (orig_w as usize, orig_h as usize);
        let (tw, th) = (tw as usize, th as usize);
        let (sw, sh) = (tw.min(orig_w / run), th.min(orig_h / run));
        let (sw, sh) = (sw.max(1), sh.max(1));
        let xmap = (0..width)
            .map(|x| {
                let (ox, w0, w1) = split(x, run, orig_w, sw);
                (ox as u16, w0 as u16, w1 as u16)
            })
            .collect();
        Self {
            width,
            height,
            run,
            orig_w,
            orig_h,
            sw,
            sh,
            tw,
            th,
            xmap,
            across: vec![0; sw * 3],
            cur: vec![0; sw * 3],
            next: vec![0; sw * 3],
            oy: 0,
            y: 0,
            out: vec![0; sw * sh * 3],
        }
    }

    /// The next row, `width` RGB pixels; rows past the last are ignored.
    pub(crate) fn push(&mut self, row: &[u8]) {
        if self.y >= self.height || row.len() < self.width * 3 {
            return;
        }
        // Across: at most 255 x orig_w a sum, then in 256ths of a level.
        self.across.fill(0);
        for (px, &(ox, w0, w1)) in row.as_chunks::<3>().0.iter().zip(&self.xmap) {
            let (ox, w0, w1) = (usize::from(ox), u32::from(w0), u32::from(w1));
            for c in 0..3 {
                let v = u32::from(px[c]);
                self.across[ox * 3 + c] += v * w0;
                if w1 > 0 {
                    self.across[(ox + 1) * 3 + c] += v * w1;
                }
            }
        }
        let total = self.orig_w as u32;
        for a in &mut self.across {
            *a = (*a * 256 + total / 2) / total;
        }
        // Down: this row's part in the output row and the next; at most
        // 65280 x orig_h a sum.
        let (oy, w0, w1) = split(self.y, self.run, self.orig_h, self.sh);
        let (w0, w1) = (w0 as u32, w1 as u32);
        debug_assert_eq!(oy, self.oy);
        for ((c, n), &a) in self.cur.iter_mut().zip(&mut self.next).zip(&self.across) {
            *c += a * w0;
            *n += a * w1;
        }
        self.y += 1;
        let end = (self.y * self.run).min(self.orig_h) * self.sh;
        if end >= (self.oy + 1) * self.orig_h || self.y == self.height {
            let total = self.orig_h as u32 * 256;
            let line = &mut self.out[self.oy * self.sw * 3..(self.oy + 1) * self.sw * 3];
            for (o, c) in line.iter_mut().zip(&self.cur) {
                *o = ((*c + total / 2) / total) as u8;
            }
            std::mem::swap(&mut self.cur, &mut self.next);
            self.next.fill(0);
            self.oy += 1;
        }
    }

    /// The picture, once every row came.
    pub(crate) fn finish(self) -> Result<Picture, Error> {
        if self.y < self.height {
            return Err(Error::Truncated);
        }
        let rgb = if (self.sw, self.sh) == (self.tw, self.th) {
            self.out
        } else {
            stretch(&self.out, self.sw, self.sh, self.tw, self.th)
        };
        Ok(Picture {
            width: self.tw as u32,
            height: self.th as u32,
            rgb,
        })
    }
}

/// Input pixel `i`, a `run`-pixel span of an `n` pixel original, shrunk
/// to `n_out` (`run` x `n_out` at most `n`): the output pixel it starts
/// in, its part there and its part in the next, of `n` an output pixel.
/// Scaled by `n_out`, the input pixel spans `[i run n_out, (i + 1) run
/// n_out)` (cut at the original's end) and output pixel `o` spans `[o n,
/// (o + 1) n)`, at least as wide, so an input pixel falls in two at most.
fn split(i: usize, run: usize, n: usize, n_out: usize) -> (usize, usize, usize) {
    let start = i * run * n_out;
    let end = ((i + 1) * run).min(n) * n_out;
    let o = (start / n).min(n_out - 1);
    let edge = (o + 1) * n;
    let first = end.min(edge).saturating_sub(start);
    let rest = end.saturating_sub(start) - first;
    if o + 1 >= n_out {
        // Past the last output pixel: nothing can be, but keep it in.
        return (o, first + rest, 0);
    }
    (o, first, rest)
}

/// Bilinear: a small picture made as large as its box, once.
fn stretch(src: &[u8], sw: usize, sh: usize, tw: usize, th: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(tw * th * 3);
    for y in 0..th {
        let (y0, y1, fy) = tap(y, sh, th);
        for x in 0..tw {
            let (x0, x1, fx) = tap(x, sw, tw);
            for c in 0..3 {
                let at = |yy: usize, xx: usize| u32::from(src[(yy * sw + xx) * 3 + c]);
                let top = at(y0, x0) * (256 - fx) + at(y0, x1) * fx;
                let bottom = at(y1, x0) * (256 - fx) + at(y1, x1) * fx;
                out.push(((top * (256 - fy) + bottom * fy + 32768) >> 16) as u8);
            }
        }
    }
    out
}

/// The two source pixels around output pixel `i` and the weight of the
/// second, in 256ths: pixel centres line up.
fn tap(i: usize, n_src: usize, n_dst: usize) -> (usize, usize, u32) {
    let pos = ((2 * i + 1) * n_src * 256 / (2 * n_dst)).saturating_sub(128);
    let i0 = (pos >> 8).min(n_src - 1);
    let i1 = (i0 + 1).min(n_src - 1);
    (i0, i1, (pos & 255) as u32)
}
