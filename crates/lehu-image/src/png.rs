//! PNG: every colour type and bit depth, not interlaced. The image data is
//! inflated a row at a time into two row buffers (this one, the one
//! above, for the filters), so only the inflater's 32 KB window and two
//! rows are held whatever the picture's size. Transparency is laid on the
//! colour the caller gives; the chunks' CRCs are not checked (zlib's own
//! checksum is).

use miniz_oxide::inflate::stream::{InflateState, inflate};
use miniz_oxide::{DataFormat, MZError, MZFlush, MZStatus};

use crate::scale::Scaler;
use crate::{Error, Picture, check_size};

pub(crate) const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

pub(crate) fn decode(data: &[u8], side: u32, bg: [u8; 3]) -> Result<Picture, Error> {
    let mut pos = SIGNATURE.len();
    let mut header: Option<Header> = None;
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut rows: Option<Rows> = None;
    loop {
        let head = data.get(pos..pos + 8).ok_or(Error::Truncated)?;
        let len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
        let kind = [head[4], head[5], head[6], head[7]];
        let body = data
            .get(pos + 8..)
            .and_then(|rest| rest.get(..len))
            .ok_or(Error::Truncated)?;
        pos += 12 + len;
        match &kind {
            b"IHDR" => {
                if header.is_some() {
                    return Err(Error::Corrupt);
                }
                header = Some(Header::parse(body)?);
            }
            b"PLTE" => {
                if body.len() % 3 != 0 || body.len() > 256 * 3 {
                    return Err(Error::Corrupt);
                }
                palette = body
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|c| [c[0], c[1], c[2], 255])
                    .collect();
            }
            b"tRNS" => {
                // The palette's alphas; a grey or RGB colour key is left
                // unread (art has none).
                for (entry, &a) in palette.iter_mut().zip(body) {
                    entry[3] = a;
                }
            }
            b"IDAT" => {
                let h = header.as_ref().ok_or(Error::Corrupt)?;
                if h.colour == 3 && palette.is_empty() {
                    return Err(Error::Corrupt);
                }
                let rows = match &mut rows {
                    Some(r) => r,
                    None => rows.insert(Rows::new(h, side)),
                };
                rows.feed(body, h, &palette, bg)?;
            }
            b"IEND" => break,
            _ => {}
        }
    }
    rows.ok_or(Error::Truncated)?.scaler.finish()
}

struct Header {
    width: usize,
    height: usize,
    depth: u8,
    colour: u8,
    /// Bytes a row holds, its filter byte not counted.
    stride: usize,
    /// Bytes a pixel holds, at least one: the filters' distance.
    bpp: usize,
}

impl Header {
    fn parse(b: &[u8]) -> Result<Self, Error> {
        let [
            w0,
            w1,
            w2,
            w3,
            h0,
            h1,
            h2,
            h3,
            depth,
            colour,
            compression,
            filter,
            interlace,
        ] = *b
        else {
            return Err(Error::Corrupt);
        };
        let width = u32::from_be_bytes([w0, w1, w2, w3]);
        let height = u32::from_be_bytes([h0, h1, h2, h3]);
        check_size(width, height)?;
        let channels = match (colour, depth) {
            (0, 1 | 2 | 4 | 8 | 16) => 1,
            (2, 8 | 16) => 3,
            (3, 1 | 2 | 4 | 8) => 1,
            (4, 8 | 16) => 2,
            (6, 8 | 16) => 4,
            _ => return Err(Error::Corrupt),
        };
        if compression != 0 || filter != 0 {
            return Err(Error::Corrupt);
        }
        if interlace != 0 {
            return Err(Error::Unsupported);
        }
        let bits = channels * usize::from(depth);
        Ok(Self {
            width: width as usize,
            height: height as usize,
            depth,
            colour,
            stride: (width as usize * bits).div_ceil(8),
            bpp: bits.div_ceil(8),
        })
    }
}

/// The image data as it inflates: the row being filled and the one
/// before it, unfiltered.
struct Rows {
    inflater: Box<InflateState>,
    /// The filter byte, then the row.
    line: Vec<u8>,
    filled: usize,
    prev: Vec<u8>,
    rgb: Vec<u8>,
    done: usize,
    ended: bool,
    scaler: Scaler,
}

impl Rows {
    fn new(h: &Header, side: u32) -> Self {
        Self {
            inflater: InflateState::new_boxed(DataFormat::Zlib),
            line: vec![0; h.stride + 1],
            filled: 0,
            prev: vec![0; h.stride],
            rgb: vec![0; h.width * 3],
            done: 0,
            ended: false,
            scaler: Scaler::new(h.width as u32, h.height as u32, side),
        }
    }

    /// One IDAT chunk's data.
    fn feed(
        &mut self,
        mut input: &[u8],
        h: &Header,
        palette: &[[u8; 4]],
        bg: [u8; 3],
    ) -> Result<(), Error> {
        while !self.ended && self.done < h.height {
            let r = inflate(
                &mut self.inflater,
                input,
                &mut self.line[self.filled..],
                MZFlush::None,
            );
            input = &input[r.bytes_consumed..];
            self.filled += r.bytes_written;
            if self.filled == self.line.len() {
                self.row(h, palette, bg)?;
                self.filled = 0;
            }
            match r.status {
                Ok(MZStatus::StreamEnd) => self.ended = true,
                Ok(_) => {
                    if r.bytes_consumed == 0 && r.bytes_written == 0 {
                        break;
                    }
                }
                // Wants more input than this chunk has.
                Err(MZError::Buf) => break,
                Err(_) => return Err(Error::Corrupt),
            }
        }
        Ok(())
    }

    /// A whole row inflated: unfilter it, make it RGB, pass it on.
    fn row(&mut self, h: &Header, palette: &[[u8; 4]], bg: [u8; 3]) -> Result<(), Error> {
        let (filter, cur) = self.line.split_first_mut().ok_or(Error::Corrupt)?;
        let prev = &self.prev;
        let bpp = h.bpp;
        match *filter {
            0 => {}
            1 => {
                for i in bpp..cur.len() {
                    cur[i] = cur[i].wrapping_add(cur[i - bpp]);
                }
            }
            2 => {
                for (c, &p) in cur.iter_mut().zip(prev) {
                    *c = c.wrapping_add(p);
                }
            }
            3 => {
                for i in 0..cur.len() {
                    let left = if i >= bpp { u16::from(cur[i - bpp]) } else { 0 };
                    cur[i] = cur[i].wrapping_add(((left + u16::from(prev[i])) / 2) as u8);
                }
            }
            4 => {
                for i in 0..cur.len() {
                    let (a, c) = if i >= bpp {
                        (cur[i - bpp], prev[i - bpp])
                    } else {
                        (0, 0)
                    };
                    cur[i] = cur[i].wrapping_add(paeth(a, prev[i], c));
                }
            }
            _ => return Err(Error::Corrupt),
        }
        to_rgb(cur, h, palette, bg, &mut self.rgb);
        self.prev.copy_from_slice(cur);
        self.scaler.push(&self.rgb);
        self.done += 1;
        Ok(())
    }
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = i16::from(a) + i16::from(b) - i16::from(c);
    let (pa, pb, pc) = (
        (p - i16::from(a)).abs(),
        (p - i16::from(b)).abs(),
        (p - i16::from(c)).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

/// A row's samples as RGB: 16-bit samples by their high byte, grey of
/// fewer bits stretched to 0-255, alpha laid on `bg`.
fn to_rgb(row: &[u8], h: &Header, palette: &[[u8; 4]], bg: [u8; 3], out: &mut [u8]) {
    let wide = h.depth == 16;
    let step = if wide { 2 } else { 1 };
    // The n-th sample of the row, its high byte when 16 bits.
    let sample = |n: usize| row.get(n * step).copied().unwrap_or(0);
    // A sample of fewer than 8 bits, as its value.
    let packed = |x: usize| {
        let d = usize::from(h.depth);
        let bit = x * d;
        let byte = row.get(bit / 8).copied().unwrap_or(0);
        (byte >> (8 - d - bit % 8)) & ((1u16 << d) - 1) as u8
    };
    let blend = |c: u8, a: u8, b: u8| {
        ((u16::from(c) * u16::from(a) + u16::from(b) * u16::from(255 - a) + 127) / 255) as u8
    };
    for (x, px) in out
        .as_chunks_mut::<3>()
        .0
        .iter_mut()
        .enumerate()
        .take(h.width)
    {
        let (rgb, a) = match h.colour {
            0 => {
                let g = if h.depth < 8 {
                    let max = (1u16 << h.depth) - 1;
                    (u16::from(packed(x)) * 255 / max) as u8
                } else {
                    sample(x)
                };
                ([g, g, g], 255)
            }
            2 => ([sample(3 * x), sample(3 * x + 1), sample(3 * x + 2)], 255),
            3 => {
                let i = if h.depth < 8 { packed(x) } else { sample(x) };
                let e = palette
                    .get(usize::from(i))
                    .copied()
                    .unwrap_or([0, 0, 0, 255]);
                ([e[0], e[1], e[2]], e[3])
            }
            4 => {
                let g = sample(2 * x);
                ([g, g, g], sample(2 * x + 1))
            }
            _ => (
                [sample(4 * x), sample(4 * x + 1), sample(4 * x + 2)],
                sample(4 * x + 3),
            ),
        };
        for c in 0..3 {
            px[c] = if a == 255 {
                rgb[c]
            } else {
                blend(rgb[c], a, bg[c])
            };
        }
    }
}
