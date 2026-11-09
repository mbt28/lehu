// SPDX-License-Identifier: GPL-3.0-or-later
//! JPEG: Huffman-coded baseline and progressive, 8-bit samples, one
//! component (grey) or three (YCbCr, or RGB when an Adobe marker says so).
//!
//! The inverse DCT runs at 1, 2, 4 or 8 pixels a block side, the least
//! that still gives the box all its pixels, so a large picture costs its
//! entropy decoding and little else. A baseline picture whose one scan
//! holds every component is put out a row of MCUs at a time; any other
//! keeps its coefficients until the last scan, within `MAX_COEFS`.

use crate::scale::Scaler;
use crate::{Error, Picture, check_size, fit};

/// Coefficients kept for a progressive picture (two bytes each): 4 MB,
/// a 1000 x 1000 picture with its colour at half resolution.
const MAX_COEFS: usize = 2 * 1024 * 1024;

/// Scans a picture may have; a progressive one has ten or so.
const MAX_SCANS: usize = 32;

/// Bytes the entropy decoder may make up past the end of a scan before
/// the scan counts as cut short.
const MAX_PADDING: u32 = 64;

/// Zig-zag position -> position in the block, row by row.
const ZIGZAG: [u8; 64] = [
    0, 1, 8, 16, 9, 2, 3, 10, 17, 24, 32, 25, 18, 11, 4, 5, 12, 19, 26, 33, 40, 48, 41, 34, 27, 20,
    13, 6, 7, 14, 21, 28, 35, 42, 49, 56, 57, 50, 43, 36, 29, 22, 15, 23, 30, 37, 44, 51, 58, 59,
    52, 45, 38, 31, 39, 46, 53, 60, 61, 54, 47, 55, 62, 63,
];

/// cos(m pi / 16) in 4096ths.
const COS: [i32; 32] = [
    4096, 4017, 3784, 3406, 2896, 2276, 1567, 799, 0, -799, -1567, -2276, -2896, -3406, -3784,
    -4017, -4096, -4017, -3784, -3406, -2896, -2276, -1567, -799, 0, 799, 1567, 2276, 2896, 3406,
    3784, 4017,
];

pub(crate) fn decode(data: &[u8], side: u32) -> Result<Picture, Error> {
    let mut d = Decoder::default();
    let mut pos = 2;
    let mut scans = 0;
    loop {
        // A marker: 0xff, any fill bytes, its code. The data ending
        // without one is taken as the end of the picture.
        if data.get(pos) != Some(&0xff) {
            match next_marker(data, pos) {
                Some(at) => pos = at,
                None => break,
            }
        }
        while data.get(pos) == Some(&0xff) {
            pos += 1;
        }
        let marker = *data.get(pos).ok_or(Error::Truncated)?;
        pos += 1;
        match marker {
            0xd9 => break,
            0xd0..=0xd7 | 0x01 => continue,
            0xd8 => return Err(Error::Corrupt),
            _ => {}
        }
        let len = usize::from(u16::from_be_bytes([
            *data.get(pos).ok_or(Error::Truncated)?,
            *data.get(pos + 1).ok_or(Error::Truncated)?,
        ]));
        if len < 2 || pos + len > data.len() {
            return Err(Error::Truncated);
        }
        let seg = &data[pos + 2..pos + len];
        pos += len;
        match marker {
            0xc0..=0xc2 => d.frame(seg, marker == 0xc2, side)?,
            0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => return Err(Error::Unsupported),
            0xc4 => d.huffman(seg)?,
            0xdb => d.quant(seg)?,
            0xdd => {
                let [a, b, ..] = seg else {
                    return Err(Error::Corrupt);
                };
                d.restart = usize::from(u16::from_be_bytes([*a, *b]));
            }
            0xee if seg.starts_with(b"Adobe") && seg.len() >= 12 => d.adobe = Some(seg[11]),
            0xda => {
                scans += 1;
                if scans > MAX_SCANS {
                    return Err(Error::TooBig);
                }
                pos = d.scan(seg, data, pos, side)?;
            }
            _ => {}
        }
    }
    d.finish()
}

/// Where the next marker starts at or after `pos`: an 0xff not followed
/// by a stuffed zero or a restart.
fn next_marker(data: &[u8], mut pos: usize) -> Option<usize> {
    while pos + 1 < data.len() {
        if data[pos] == 0xff && !matches!(data[pos + 1], 0x00 | 0xd0..=0xd7 | 0xff) {
            return Some(pos);
        }
        pos += 1;
    }
    None
}

#[derive(Default)]
struct Decoder {
    frame: Option<Frame>,
    quant: [Option<[u16; 64]>; 4],
    dc: [Option<Box<Huffman>>; 4],
    ac: [Option<Box<Huffman>>; 4],
    /// MCUs between restart markers; 0 for none.
    restart: usize,
    /// The Adobe marker's colour transform: 0 is RGB.
    adobe: Option<u8>,
}

struct Frame {
    width: usize,
    height: usize,
    progressive: bool,
    comps: Vec<Component>,
    hmax: usize,
    vmax: usize,
    mcux: usize,
    mcuy: usize,
    /// Pixels a block side, as decoded: 1, 2, 4 or 8.
    scale: usize,
    /// The picture's rows out, once its first scan says how.
    out: Option<Out>,
    /// The tables and the colour transform as of the latest scan.
    quant: [Option<[u16; 64]>; 4],
    adobe: Option<u8>,
    /// Scans decoded.
    scans: usize,
}

struct Component {
    id: u8,
    h: usize,
    v: usize,
    tq: usize,
    /// Blocks across the coefficient store, the MCUs' padding included.
    bw: usize,
    /// Samples a block side as transformed, across and down: as many as
    /// the picture's pixels at `Frame::scale` need of this component (its
    /// colour at the luma's resolution, when the DCT can give that).
    sx: usize,
    sy: usize,
    /// The store's coefficients, block by block, each in row order.
    coef: Vec<i16>,
    pred: i32,
}

/// How rows leave: streaming a row of MCUs at a time, or all at the end.
struct Out {
    streaming: bool,
    scaler: Scaler,
    /// Each component's samples for one row of MCUs, as decoded.
    planes: Vec<Vec<u8>>,
    row: Vec<u8>,
}

impl Decoder {
    fn frame(&mut self, seg: &[u8], progressive: bool, side: u32) -> Result<(), Error> {
        if self.frame.is_some() {
            return Err(Error::Corrupt);
        }
        let [precision, h1, h0, w1, w0, n, rest @ ..] = seg else {
            return Err(Error::Corrupt);
        };
        if *precision != 8 {
            return Err(Error::Unsupported);
        }
        let height = u32::from(u16::from_be_bytes([*h1, *h0]));
        let width = u32::from(u16::from_be_bytes([*w1, *w0]));
        // A height of 0 waits for a DNL marker, which nothing sends.
        check_size(width, height)?;
        let n = usize::from(*n);
        if !(n == 1 || n == 3) {
            return Err(if n == 4 {
                Error::Unsupported
            } else {
                Error::Corrupt
            });
        }
        if rest.len() < n * 3 {
            return Err(Error::Corrupt);
        }
        let mut comps = Vec::with_capacity(n);
        for c in rest.as_chunks::<3>().0.iter().take(n) {
            let (h, v, tq) = (
                usize::from(c[1] >> 4),
                usize::from(c[1] & 15),
                usize::from(c[2]),
            );
            if !(1..=4).contains(&h) || !(1..=4).contains(&v) || tq > 3 {
                return Err(Error::Corrupt);
            }
            if comps.iter().any(|k: &Component| k.id == c[0]) {
                return Err(Error::Corrupt);
            }
            // One component: its sampling is the picture's, whatever it says.
            let (h, v) = if n == 1 { (1, 1) } else { (h, v) };
            comps.push(Component {
                id: c[0],
                h,
                v,
                tq,
                bw: 0,
                sx: 8,
                sy: 8,
                coef: Vec::new(),
                pred: 0,
            });
        }
        let hmax = comps.iter().map(|c| c.h).max().unwrap_or(1);
        let vmax = comps.iter().map(|c| c.v).max().unwrap_or(1);
        let (width, height) = (width as usize, height as usize);
        let mcux = width.div_ceil(8 * hmax);
        let mcuy = height.div_ceil(8 * vmax);
        for c in &mut comps {
            c.bw = mcux * c.h;
        }
        // The fewest pixels a block that still fill the box, whole pixels
        // of the picture's own.
        let (tw, th) = fit(width as u32, height as u32, side);
        let scale = [1, 2, 4, 8]
            .into_iter()
            .find(|&s| width * s >= 8 * tw as usize && height * s >= 8 * th as usize)
            .unwrap_or(8);
        for c in &mut comps {
            // A ratio the DCT sizes cannot make is sampled from the size
            // the luma has.
            let own = |max: usize, f: usize| {
                if max.is_multiple_of(f) {
                    (scale * max / f).min(8)
                } else {
                    scale
                }
            };
            (c.sx, c.sy) = (own(hmax, c.h), own(vmax, c.v));
        }
        self.frame = Some(Frame {
            width,
            height,
            progressive,
            comps,
            hmax,
            vmax,
            mcux,
            mcuy,
            scale,
            out: None,
            quant: [None; 4],
            adobe: None,
            scans: 0,
        });
        Ok(())
    }

    fn quant(&mut self, mut seg: &[u8]) -> Result<(), Error> {
        while let [pq_tq, rest @ ..] = seg {
            let (wide, id) = (pq_tq >> 4, usize::from(pq_tq & 15));
            let size = if wide == 1 { 128 } else { 64 };
            if wide > 1 || id > 3 || rest.len() < size {
                return Err(Error::Corrupt);
            }
            let mut table = [0u16; 64];
            for (k, &z) in ZIGZAG.iter().enumerate() {
                table[usize::from(z)] = if wide == 1 {
                    u16::from_be_bytes([rest[2 * k], rest[2 * k + 1]])
                } else {
                    u16::from(rest[k])
                };
            }
            self.quant[id] = Some(table);
            seg = &rest[size..];
        }
        Ok(())
    }

    fn huffman(&mut self, mut seg: &[u8]) -> Result<(), Error> {
        while let [tc_th, rest @ ..] = seg {
            let (class, id) = (tc_th >> 4, usize::from(tc_th & 15));
            if class > 1 || id > 3 || rest.len() < 16 {
                return Err(Error::Corrupt);
            }
            let counts: [u8; 16] = rest[..16].try_into().map_err(|_| Error::Corrupt)?;
            let total: usize = counts.iter().map(|&c| usize::from(c)).sum();
            if total > 256 || rest.len() < 16 + total {
                return Err(Error::Corrupt);
            }
            let table = Huffman::new(&counts, &rest[16..16 + total])?;
            if class == 0 {
                self.dc[id] = Some(table);
            } else {
                self.ac[id] = Some(table);
            }
            seg = &rest[16 + total..];
        }
        Ok(())
    }

    /// One scan: its header in `seg`, its entropy-coded data from `pos`.
    /// Where the next marker is.
    fn scan(&mut self, seg: &[u8], data: &[u8], pos: usize, side: u32) -> Result<usize, Error> {
        let frame = self.frame.as_mut().ok_or(Error::Corrupt)?;
        frame.quant = self.quant;
        frame.adobe = self.adobe;
        let [n, rest @ ..] = seg else {
            return Err(Error::Corrupt);
        };
        let n = usize::from(*n);
        if n == 0 || n > frame.comps.len() || rest.len() < 2 * n + 3 {
            return Err(Error::Corrupt);
        }
        let mut sel = [(0usize, 0usize, 0usize); 3];
        for (i, c) in rest.as_chunks::<2>().0.iter().take(n).enumerate() {
            let k = frame
                .comps
                .iter()
                .position(|k| k.id == c[0])
                .ok_or(Error::Corrupt)?;
            if sel[..i].iter().any(|s| s.0 == k) {
                return Err(Error::Corrupt);
            }
            let (td, ta) = (usize::from(c[1] >> 4), usize::from(c[1] & 15));
            if td > 3 || ta > 3 {
                return Err(Error::Corrupt);
            }
            sel[i] = (k, td, ta);
        }
        let sel = &sel[..n];
        let p = &rest[2 * n..];
        let (mut ss, mut se) = (usize::from(p[0]), usize::from(p[1]));
        let (ah, al) = (u32::from(p[2] >> 4), u32::from(p[2] & 15));
        if frame.progressive {
            let dc_ok = ss == 0 && se == 0;
            let ac_ok = ss >= 1 && ss <= se && se <= 63 && n == 1;
            if !(dc_ok || ac_ok) || ah > 13 || al > 13 {
                return Err(Error::Corrupt);
            }
        } else {
            (ss, se) = (0, 63);
        }
        let kind = match (frame.progressive, ss, ah) {
            (false, _, _) => Kind::Baseline,
            (true, 0, 0) => Kind::DcFirst,
            (true, 0, _) => Kind::DcRefine,
            (true, _, 0) => Kind::AcFirst,
            (true, _, _) => Kind::AcRefine,
        };
        // The tables the scan uses must be there before its data.
        for &(_, td, ta) in sel {
            let dc_needed = matches!(kind, Kind::Baseline | Kind::DcFirst);
            let ac_needed = matches!(kind, Kind::Baseline | Kind::AcFirst | Kind::AcRefine);
            if (dc_needed && self.dc[td].is_none()) || (ac_needed && self.ac[ta].is_none()) {
                return Err(Error::Corrupt);
            }
        }
        if frame.out.is_none() {
            // Every component in the one baseline scan: rows out as they
            // come. Otherwise every coefficient is kept to the end.
            let streaming = kind == Kind::Baseline && n == frame.comps.len();
            frame.start(streaming, side)?;
        }
        let streaming = frame.out.as_ref().is_some_and(|o| o.streaming);
        if streaming && frame.scans > 0 {
            // A second scan of a picture already put out.
            return Err(Error::Corrupt);
        }
        frame.scans += 1;
        for c in &mut frame.comps {
            c.pred = 0;
        }
        let tables = Tables {
            dc: &self.dc,
            ac: &self.ac,
        };
        let mut bits = Bits::new(data, pos);
        let mut eobrun = 0u32;
        let spec = Spectral { ss, se, al };
        let restart = self.restart;
        let mut units = 0usize;
        if n == 1 {
            // One component: its blocks in raster order, its own extent.
            let (k, td, ta) = sel[0];
            let c = &frame.comps[k];
            let bx_n = (frame.width * c.h).div_ceil(frame.hmax).div_ceil(8);
            let by_n = (frame.height * c.v).div_ceil(frame.vmax).div_ceil(8);
            let rows_kept = if streaming { c.v } else { frame.mcuy * c.v };
            for by in 0..by_n {
                for bx in 0..bx_n {
                    if restart_due(restart, &mut units) {
                        bits.restart();
                        frame.comps[k].pred = 0;
                        eobrun = 0;
                    }
                    let c = &mut frame.comps[k];
                    let at = ((by % rows_kept) * c.bw + bx) * 64;
                    let block = &mut c.coef[at..at + 64];
                    block_decode(
                        &mut bits,
                        &tables,
                        kind,
                        &spec,
                        td,
                        ta,
                        &mut c.pred,
                        &mut eobrun,
                        block,
                    )?;
                }
                if streaming && (by + 1) % frame.comps[k].v == 0 {
                    frame.emit(by / frame.comps[k].v)?;
                }
            }
        } else {
            for my in 0..frame.mcuy {
                for mx in 0..frame.mcux {
                    if restart_due(restart, &mut units) {
                        bits.restart();
                        for c in &mut frame.comps {
                            c.pred = 0;
                        }
                        eobrun = 0;
                    }
                    for &(k, td, ta) in sel {
                        let c = &mut frame.comps[k];
                        let base_row = if streaming { 0 } else { my * c.v };
                        for v in 0..c.v {
                            for h in 0..c.h {
                                let at = ((base_row + v) * c.bw + mx * c.h + h) * 64;
                                let block = &mut c.coef[at..at + 64];
                                block_decode(
                                    &mut bits,
                                    &tables,
                                    kind,
                                    &spec,
                                    td,
                                    ta,
                                    &mut c.pred,
                                    &mut eobrun,
                                    block,
                                )?;
                            }
                        }
                    }
                }
                if streaming {
                    frame.emit(my)?;
                }
            }
        }
        Ok(next_marker(data, bits.pos).unwrap_or(data.len()))
    }

    fn finish(mut self) -> Result<Picture, Error> {
        let mut frame = self.frame.take().ok_or(Error::Corrupt)?;
        let Some(out) = frame.out.as_ref() else {
            return Err(Error::Truncated);
        };
        if !out.streaming {
            // Dequantised and transformed only now: the tables may come
            // between scans.
            frame.quant = self.quant;
            for my in 0..frame.mcuy {
                frame.emit(my)?;
            }
        }
        let out = frame.out.take().ok_or(Error::Corrupt)?;
        out.scaler.finish()
    }
}

/// Whether a restart marker comes before this unit (an MCU, or a block
/// of a one-component scan): one between every `restart` of them.
fn restart_due(restart: usize, units: &mut usize) -> bool {
    let due = restart > 0 && *units > 0 && units.is_multiple_of(restart);
    *units += 1;
    due
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Baseline,
    DcFirst,
    DcRefine,
    AcFirst,
    AcRefine,
}

/// The band of coefficients a scan carries and the bit it starts at.
struct Spectral {
    ss: usize,
    se: usize,
    al: u32,
}

struct Tables<'a> {
    dc: &'a [Option<Box<Huffman>>; 4],
    ac: &'a [Option<Box<Huffman>>; 4],
}

impl Frame {
    /// Allocate the coefficient store and the rows out.
    fn start(&mut self, streaming: bool, side: u32) -> Result<(), Error> {
        let mut total = 0usize;
        for c in &self.comps {
            let rows = if streaming { c.v } else { self.mcuy * c.v };
            total += c.bw * rows * 64;
        }
        if !streaming && total > MAX_COEFS {
            return Err(Error::TooBig);
        }
        for c in &mut self.comps {
            let rows = if streaming { c.v } else { self.mcuy * c.v };
            c.coef = vec![0; c.bw * rows * 64];
        }
        let s = self.scale;
        let out_w = (self.width * s).div_ceil(8);
        let out_h = (self.height * s).div_ceil(8);
        let planes = self
            .comps
            .iter()
            .map(|c| vec![0u8; c.bw * c.sx * c.v * c.sy])
            .collect();
        self.out = Some(Out {
            streaming,
            scaler: Scaler::shrunk(
                out_w as u32,
                out_h as u32,
                (8 / s) as u32,
                self.width as u32,
                self.height as u32,
                side,
            ),
            planes,
            row: vec![0; out_w * 3],
        });
        Ok(())
    }
}

/// Decode one block's part of a scan into `block` (row order).
#[allow(clippy::too_many_arguments)]
fn block_decode(
    bits: &mut Bits<'_>,
    t: &Tables<'_>,
    kind: Kind,
    spec: &Spectral,
    td: usize,
    ta: usize,
    pred: &mut i32,
    eobrun: &mut u32,
    block: &mut [i16],
) -> Result<(), Error> {
    if bits.padding > MAX_PADDING {
        return Err(Error::Truncated);
    }
    // The scan checked its tables before its data.
    let (dc, ac) = (t.dc[td].as_deref(), t.ac[ta].as_deref());
    match kind {
        Kind::Baseline => {
            let (Some(dc), Some(ac)) = (dc, ac) else {
                return Err(Error::Corrupt);
            };
            block.fill(0);
            let s = u32::from(dc.decode(bits)?);
            *pred = pred.wrapping_add(bits.receive_extend(s)?);
            block[0] = clamp16(*pred);
            let mut k = 1;
            while k < 64 {
                let rs = ac.decode(bits)?;
                let (r, s) = (usize::from(rs >> 4), u32::from(rs & 15));
                if s == 0 {
                    if r != 15 {
                        break;
                    }
                    k += 16;
                    continue;
                }
                k += r;
                if k > 63 {
                    return Err(Error::Corrupt);
                }
                block[usize::from(ZIGZAG[k])] = clamp16(bits.receive_extend(s)?);
                k += 1;
            }
        }
        Kind::DcFirst => {
            let dc = dc.ok_or(Error::Corrupt)?;
            let s = u32::from(dc.decode(bits)?);
            *pred = pred.wrapping_add(bits.receive_extend(s)?);
            block[0] = clamp16(pred.wrapping_shl(spec.al));
        }
        Kind::DcRefine => {
            if bits.bit() {
                block[0] |= 1 << spec.al;
            }
        }
        Kind::AcFirst => {
            let ac = ac.ok_or(Error::Corrupt)?;
            if *eobrun > 0 {
                *eobrun -= 1;
                return Ok(());
            }
            let mut k = spec.ss;
            while k <= spec.se {
                let rs = ac.decode(bits)?;
                let (r, s) = (u32::from(rs >> 4), u32::from(rs & 15));
                if s == 0 {
                    if r < 15 {
                        *eobrun = (1 << r) - 1;
                        if r > 0 {
                            *eobrun += bits.receive(r);
                        }
                        break;
                    }
                    k += 16;
                    continue;
                }
                k += r as usize;
                if k > 63 {
                    return Err(Error::Corrupt);
                }
                block[usize::from(ZIGZAG[k])] =
                    clamp16(bits.receive_extend(s)?.wrapping_shl(spec.al));
                k += 1;
            }
        }
        Kind::AcRefine => ac_refine(bits, ac.ok_or(Error::Corrupt)?, spec, eobrun, block)?,
    }
    Ok(())
}

/// A refinement scan of the AC band: one more bit of every coefficient
/// already seen, and the ones that become non-zero now (G.1.2.3).
fn ac_refine(
    bits: &mut Bits<'_>,
    ac: &Huffman,
    spec: &Spectral,
    eobrun: &mut u32,
    block: &mut [i16],
) -> Result<(), Error> {
    let p1: i16 = 1 << spec.al;
    let m1: i16 = -p1;
    let refine = |bits: &mut Bits<'_>, c: &mut i16| {
        if bits.bit() && *c & p1 == 0 {
            *c = c.saturating_add(if *c >= 0 { p1 } else { m1 });
        }
    };
    let mut k = spec.ss;
    if *eobrun == 0 {
        while k <= spec.se {
            let rs = ac.decode(bits)?;
            let (mut r, s) = (i32::from(rs >> 4), rs & 15);
            let mut value = 0i16;
            if s != 0 {
                if s != 1 {
                    return Err(Error::Corrupt);
                }
                value = if bits.bit() { p1 } else { m1 };
            } else if r != 15 {
                *eobrun = 1 << r;
                if r > 0 {
                    *eobrun += bits.receive(r as u32);
                }
                break;
            }
            // Past `r` zero coefficients, refining the non-zero ones on
            // the way; the new one goes in the zero after them.
            while k <= spec.se {
                let c = &mut block[usize::from(ZIGZAG[k])];
                if *c != 0 {
                    refine(bits, c);
                } else {
                    r -= 1;
                    if r < 0 {
                        break;
                    }
                }
                k += 1;
            }
            if value != 0 && k <= spec.se {
                block[usize::from(ZIGZAG[k])] = value;
            }
            k += 1;
        }
    }
    if *eobrun > 0 {
        // The band's end in a run of ends: only refinement bits.
        while k <= spec.se {
            let c = &mut block[usize::from(ZIGZAG[k])];
            if *c != 0 {
                refine(bits, c);
            }
            k += 1;
        }
        *eobrun -= 1;
    }
    Ok(())
}

fn clamp16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

impl Frame {
    /// One row of MCUs out: each block transformed, the components put
    /// together into RGB rows, the rows to the scaler.
    fn emit(&mut self, my: usize) -> Result<(), Error> {
        let s = self.scale;
        let rgb = self.comps.len() == 3 && self.rgb_transform();
        let Some(out) = self.out.as_mut() else {
            return Err(Error::Corrupt);
        };
        for (c, plane) in self.comps.iter().zip(out.planes.iter_mut()) {
            let quant = self.quant[c.tq].as_ref().ok_or(Error::Corrupt)?;
            let base_row = if out.streaming { 0 } else { my * c.v };
            let pw = c.bw * c.sx;
            let (kx, ky) = (kernel(c.sx), kernel(c.sy));
            for v in 0..c.v {
                for bx in 0..c.bw {
                    let at = ((base_row + v) * c.bw + bx) * 64;
                    let block = &c.coef[at..at + 64];
                    let dst = &mut plane[v * c.sy * pw + bx * c.sx..];
                    idct(block, quant, (c.sx, c.sy), (&kx, &ky), dst, pw);
                }
            }
        }
        let out_w = (self.width * s).div_ceil(8);
        let out_h = (self.height * s).div_ceil(8);
        let rows = self.vmax * s;
        for y in 0..rows {
            let gy = my * rows + y;
            if gy >= out_h {
                break;
            }
            let at = |k: usize, x: usize| {
                let c = &self.comps[k];
                let pw = c.bw * c.sx;
                let py = y * c.v * c.sy / (self.vmax * s);
                out.planes[k][py * pw + x * c.h * c.sx / (self.hmax * s)]
            };
            for x in 0..out_w {
                let px = &mut out.row[x * 3..x * 3 + 3];
                if self.comps.len() == 1 {
                    px.fill(at(0, x));
                } else if rgb {
                    px.copy_from_slice(&[at(0, x), at(1, x), at(2, x)]);
                } else {
                    px.copy_from_slice(&ycc_rgb(at(0, x), at(1, x), at(2, x)));
                }
            }
            out.scaler.push(&out.row);
        }
        Ok(())
    }
}

/// The inverse DCT's basis averaged over each run of 8/s pixels, in
/// 4096ths: `[x * 8 + u]`, the DC term's 1/sqrt(2) folded in. With it a
/// block transforms straight to the means of its `s` x `s` squares, every
/// frequency counted, as if decoded whole and then shrunk.
fn kernel(s: usize) -> [i32; 64] {
    let mut k = [0i32; 64];
    let run = 8 / s;
    for x in 0..s {
        for u in 0..8 {
            k[x * 8 + u] = if u == 0 {
                2896
            } else {
                let sum: i32 = (x * run..(x + 1) * run)
                    .map(|p| COS[((2 * p + 1) * u) % 32])
                    .sum();
                (sum + sum.signum() * run as i32 / 2) / run as i32
            };
        }
    }
    k
}

/// Dequantise and transform a block (row order) into `sx` x `sy` samples
/// at `out`, rows `stride` apart, with the kernels of those sizes. 64-bit
/// sums, so no coefficient a file holds can overflow them.
fn idct(
    block: &[i16],
    quant: &[u16; 64],
    (sx, sy): (usize, usize),
    (kx, ky): (&[i32; 64], &[i32; 64]),
    out: &mut [u8],
    stride: usize,
) {
    let mut tmp = [0i64; 64];
    for v in 0..8 {
        let row = &block[v * 8..v * 8 + 8];
        if row.iter().all(|&c| c == 0) {
            continue;
        }
        let mut f = [0i64; 8];
        for u in 0..8 {
            f[u] = i64::from(row[u]) * i64::from(quant[v * 8 + u]);
        }
        for x in 0..sx {
            let mut acc = 0i64;
            for u in 0..8 {
                acc += f[u] * i64::from(kx[x * 8 + u]);
            }
            tmp[v * 8 + x] = acc >> 3;
        }
    }
    for y in 0..sy {
        for x in 0..sx {
            let mut acc = 0i64;
            for v in 0..8 {
                acc += tmp[v * 8 + x] * i64::from(ky[y * 8 + v]);
            }
            let value = ((acc + (1 << 22)) >> 23) + 128;
            out[y * stride + x] = value.clamp(0, 255) as u8;
        }
    }
}

/// JFIF's YCbCr to RGB, in 65536ths.
fn ycc_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let y = i32::from(y) << 16;
    let (cb, cr) = (i32::from(cb) - 128, i32::from(cr) - 128);
    let c = |v: i32| ((v + 32768) >> 16).clamp(0, 255) as u8;
    [
        c(y + 91881 * cr),
        c(y - 22553 * cb - 46802 * cr),
        c(y + 116130 * cb),
    ]
}

impl Frame {
    /// Three components that are R, G and B rather than YCbCr: an Adobe
    /// marker's transform 0, or ids that spell it.
    fn rgb_transform(&self) -> bool {
        match self.adobe {
            Some(t) => t == 0,
            None => self.comps.iter().map(|c| c.id).eq(*b"RGB"),
        }
    }
}

/// A Huffman table: a 9-bit lookup for the short codes, the code ranges
/// of each length for the long ones (F.2.2.3).
struct Huffman {
    /// `(length << 8) | symbol` for codes of up to `LOOKUP` bits; 0 when
    /// longer.
    lookup: [u16; 1 << LOOKUP],
    maxcode: [i32; 17],
    offset: [i32; 17],
    symbols: [u8; 256],
}

const LOOKUP: u32 = 9;

impl Huffman {
    fn new(counts: &[u8; 16], symbols: &[u8]) -> Result<Box<Self>, Error> {
        let mut t = Box::new(Self {
            lookup: [0; 1 << LOOKUP],
            maxcode: [-1; 17],
            offset: [0; 17],
            symbols: [0; 256],
        });
        t.symbols[..symbols.len()].copy_from_slice(symbols);
        let mut code = 0i32;
        let mut k = 0usize;
        for len in 1..=16 {
            let n = i32::from(counts[len - 1]);
            t.offset[len] = k as i32 - code;
            // More codes of this length than it has: not a prefix code.
            if code + n > 1 << len {
                return Err(Error::Corrupt);
            }
            if n > 0 {
                for i in 0..n as usize {
                    if len as u32 <= LOOKUP {
                        let c = (code as usize + i) << (LOOKUP as usize - len);
                        let entry = ((len as u16) << 8) | u16::from(symbols[k + i]);
                        for e in &mut t.lookup[c..c + (1 << (LOOKUP as usize - len))] {
                            *e = entry;
                        }
                    }
                }
                code += n;
                k += n as usize;
                t.maxcode[len] = code - 1;
            }
            code <<= 1;
        }
        Ok(t)
    }

    fn decode(&self, bits: &mut Bits<'_>) -> Result<u8, Error> {
        bits.fill();
        let peek = (bits.acc >> (32 - LOOKUP)) as usize;
        let entry = self.lookup[peek];
        if entry != 0 {
            bits.consume(u32::from(entry >> 8));
            return Ok(entry as u8);
        }
        let mut code = 0i32;
        for len in 1..=16 {
            code = (code << 1) | i32::from(bits.bit());
            if code <= self.maxcode[len] {
                let at = (self.offset[len] + code) as usize;
                return self.symbols.get(at).copied().ok_or(Error::Corrupt);
            }
        }
        Err(Error::Corrupt)
    }
}

/// The entropy-coded bits, most significant first: stuffed zeros taken
/// out; at a marker or the end, zeros are made up and counted.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    /// The next bits, from the top.
    acc: u32,
    n: u32,
    /// Bytes made up past the data.
    padding: u32,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], pos: usize) -> Self {
        Self {
            data,
            pos,
            acc: 0,
            n: 0,
            padding: 0,
        }
    }

    fn fill(&mut self) {
        while self.n <= 24 {
            let byte = match self.data.get(self.pos) {
                Some(&0xff) if self.data.get(self.pos + 1) == Some(&0) => {
                    self.pos += 2;
                    0xff
                }
                Some(&0xff) | None => {
                    self.padding += 1;
                    0
                }
                Some(&b) => {
                    self.pos += 1;
                    b
                }
            };
            self.acc |= u32::from(byte) << (24 - self.n);
            self.n += 8;
        }
    }

    fn consume(&mut self, k: u32) {
        self.acc <<= k;
        self.n -= k;
    }

    fn bit(&mut self) -> bool {
        self.fill();
        let b = self.acc >> 31 != 0;
        self.consume(1);
        b
    }

    /// `k` bits (at most 16) as a number.
    fn receive(&mut self, k: u32) -> u32 {
        if k == 0 {
            return 0;
        }
        self.fill();
        let v = self.acc >> (32 - k);
        self.consume(k);
        v
    }

    /// `s` bits as a signed difference (F.2.2.1); more than 16 is no
    /// category a file can hold.
    fn receive_extend(&mut self, s: u32) -> Result<i32, Error> {
        if s == 0 {
            return Ok(0);
        }
        if s > 16 {
            return Err(Error::Corrupt);
        }
        let v = self.receive(s) as i32;
        Ok(if v < 1 << (s - 1) {
            v - (1 << s) + 1
        } else {
            v
        })
    }

    /// At a restart marker: the bits left go, the marker is passed.
    fn restart(&mut self) {
        self.acc = 0;
        self.n = 0;
        self.padding = 0;
        if let Some(at) = (self.pos..self.data.len().saturating_sub(1))
            .find(|&i| self.data[i] == 0xff && self.data[i + 1] != 0 && self.data[i + 1] != 0xff)
            && (0xd0..=0xd7).contains(&self.data[at + 1])
        {
            self.pos = at + 2;
        }
    }
}
