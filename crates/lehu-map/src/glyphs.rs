// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Names as pictures: each glyph of LEHU's font rasterized once a size,
//! and each name laid out from them, with its halo, once, kept while it is
//! drawn.
//!
//! The rasterizer is swash's, which Slint's software renderer links in
//! already. Its arithmetic is floating point, a glyph at a time: that is
//! done once a glyph and size, never a pixel at a time when the map is
//! drawn. A name's picture is upright and has one byte a pixel, an index
//! into the lumas it is drawn with (`nv12::label_lumas`), so the day's
//! and the night's colours share it.

use std::collections::HashMap;

use swash::FontRef;
use swash::scale::{Render, ScaleContext, Source};
use swash::zeno::Format;

use crate::nv12::Stamp;

/// LEHU's own font (Open Sans, SIL Open Font License 1.1).
static FONT: &[u8] = include_bytes!("../../lehu-ui/ui/fonts/font.ttf");

/// The halo round the text, in pixels: every pixel within this of the
/// text's ink (by its middle; the square of the distance at most
/// `HALO_R2`).
pub const HALO: i32 = 2;
const HALO_R2: i32 = 5;

/// Glyphs kept; past this they are all let go and made again as needed.
const GLYPHS: usize = 600;

/// The bytes of names' pictures kept, the least recently used let go
/// first.
const SPRITE_BYTES: usize = 512 * 1024;

/// A glyph at one size: its coverage, `w` x `h` bytes with its top left
/// `left` right of the pen and `top` above the baseline; its halo, the
/// coverage widened by `HALO` each way (1 or 0); and its advance, in 1/64
/// pixels.
struct Glyph {
    left: i32,
    top: i32,
    w: usize,
    h: usize,
    cover: Box<[u8]>,
    halo: Box<[u8]>,
    advance: i32,
}

/// A name's picture, as `nv12::stamp` takes it.
pub struct Sprite {
    pub w: usize,
    pub h: usize,
    pub px: Box<[u8]>,
    /// The middle of the name's box, in 1/16 pixels of the picture: the
    /// point put at the label's place.
    pub anchor: (i64, i64),
}

impl Sprite {
    pub fn stamp(&self) -> Stamp<'_> {
        Stamp {
            w: self.w,
            h: self.h,
            px: &self.px,
            anchor: self.anchor,
        }
    }
}

struct Kept {
    size: u8,
    text: Box<str>,
    used: u64,
    sprite: Sprite,
}

/// The font, its glyphs at the sizes asked for, and the names drawn.
pub struct Glyphs {
    font: FontRef<'static>,
    context: ScaleContext,
    glyphs: HashMap<(u8, char), Glyph>,
    kept: Vec<Kept>,
    bytes: usize,
    clock: u64,
}

impl Glyphs {
    /// None when the font will not open (it is built in, so never).
    pub fn new() -> Option<Self> {
        Some(Self {
            font: FontRef::from_index(FONT, 0)?,
            context: ScaleContext::new(),
            glyphs: HashMap::new(),
            kept: Vec::new(),
            bytes: 0,
            clock: 0,
        })
    }

    /// The glyph for `ch` at `size` pixels, rasterized if it is not kept.
    fn glyph(&mut self, size: u8, ch: char) -> &Glyph {
        if !self.glyphs.contains_key(&(size, ch)) {
            if self.glyphs.len() >= GLYPHS {
                self.glyphs.clear();
            }
            let g = self.rasterize(size, ch);
            self.glyphs.insert((size, ch), g);
        }
        &self.glyphs[&(size, ch)]
    }

    fn rasterize(&mut self, size: u8, ch: char) -> Glyph {
        let id = self.font.charmap().map(ch);
        let ppem = f32::from(size);
        let advance = self.font.glyph_metrics(&[]).scale(ppem).advance_width(id);
        let mut scaler = self.context.builder(self.font).size(ppem).build();
        let image = Render::new(&[Source::Outline])
            .format(Format::Alpha)
            .render(&mut scaler, id);
        let (left, top, w, h, cover) = match image {
            Some(i) if i.placement.width > 0 && i.placement.height > 0 => {
                let p = i.placement;
                let (w, h) = (p.width as usize, p.height as usize);
                let mut data = i.data;
                data.resize(w * h, 0);
                (p.left, p.top, w, h, data.into_boxed_slice())
            }
            _ => (0, 0, 0, 0, Box::default()),
        };
        Glyph {
            left,
            top,
            w,
            h,
            halo: widen(&cover, w, h),
            cover,
            advance: (advance * 64.0).round() as i32,
        }
    }

    /// The picture of `text` at `size` pixels is kept.
    pub fn has(&self, text: &str, size: u8) -> bool {
        self.kept.iter().any(|k| k.size == size && *k.text == *text)
    }

    /// The picture of `text` at `size` pixels, made if it is not kept.
    pub fn sprite(&mut self, text: &str, size: u8) -> &Sprite {
        self.clock += 1;
        let i = match self
            .kept
            .iter()
            .position(|k| k.size == size && *k.text == *text)
        {
            Some(i) => i,
            None => {
                let sprite = self.lay_out(text, size);
                let bytes = sprite.px.len();
                // Room made, the least recently used out first; never the
                // new one.
                while self.bytes + bytes > SPRITE_BYTES && !self.kept.is_empty() {
                    let old = (0..self.kept.len())
                        .min_by_key(|&i| self.kept[i].used)
                        .unwrap_or(0);
                    self.bytes -= self.kept.swap_remove(old).sprite.px.len();
                }
                self.bytes += bytes;
                self.kept.push(Kept {
                    size,
                    text: text.into(),
                    used: 0,
                    sprite,
                });
                self.kept.len() - 1
            }
        };
        self.kept[i].used = self.clock;
        &self.kept[i].sprite
    }

    /// `text` laid out on one line from the glyphs' advances (no kerning,
    /// as `scripts/map-tiles.py` measures it), the halo and then the text
    /// over it. The name's box is its advance wide and `size` high, from
    /// 0.75 of it above the baseline to 0.25 below.
    fn lay_out(&mut self, text: &str, size: u8) -> Sprite {
        // The pen's places (1/64 px) and the ink's extent with the halo,
        // pixels from the start of the baseline.
        let mut pen = 0i32;
        let mut at = Vec::with_capacity(text.len());
        let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
        for ch in text.chars() {
            let g = self.glyph(size, ch);
            let gx = ((pen + 32) >> 6) + g.left;
            if g.w > 0 {
                x0 = x0.min(gx - HALO);
                x1 = x1.max(gx + g.w as i32 + HALO);
                y0 = y0.min(-g.top - HALO);
                y1 = y1.max(-g.top + g.h as i32 + HALO);
            }
            at.push((ch, gx));
            pen += g.advance;
        }
        if x0 >= x1 {
            return Sprite {
                w: 0,
                h: 0,
                px: Box::default(),
                anchor: (0, 0),
            };
        }
        let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let mut cover = vec![0u8; w * h];
        let mut halo = vec![0u8; w * h];
        for (ch, gx) in at {
            let g = &self.glyphs[&(size, ch)];
            if g.w == 0 {
                continue;
            }
            // The glyph's top left in the picture, and its halo's.
            let (ox, oy) = ((gx - x0) as usize, (-g.top - y0) as usize);
            for r in 0..g.h {
                let src = &g.cover[r * g.w..(r + 1) * g.w];
                let o = (oy + r) * w + ox;
                for (d, &v) in cover[o..o + g.w].iter_mut().zip(src) {
                    *d = (*d).max(v);
                }
            }
            let hw = g.w + 2 * HALO as usize;
            let (hx, hy) = (ox - HALO as usize, oy - HALO as usize);
            for r in 0..g.h + 2 * HALO as usize {
                let src = &g.halo[r * hw..(r + 1) * hw];
                let o = (hy + r) * w + hx;
                for (d, &v) in halo[o..o + hw].iter_mut().zip(src) {
                    *d |= v;
                }
            }
        }
        let px: Box<[u8]> = cover
            .iter()
            .zip(&halo)
            .map(|(&c, &h)| {
                if h == 0 && c == 0 {
                    0
                } else {
                    1 + ((u32::from(c) * 254 + 127) / 255) as u8
                }
            })
            .collect();
        // The box's middle: half the advance along, a quarter of the size
        // above the baseline; in 1/16 px of the picture.
        let anchor = (
            i64::from(pen) / 8 - i64::from(x0) * 16,
            -4 * i64::from(size) - i64::from(y0) * 16,
        );
        Sprite { w, h, px, anchor }
    }
}

/// A glyph's halo: `HALO` wider each way than its coverage (`w` x `h`),
/// 1 within reach of any of its ink.
fn widen(cover: &[u8], w: usize, h: usize) -> Box<[u8]> {
    let r = HALO as usize;
    let hw = w + 2 * r;
    let mut halo = vec![0u8; hw * (h + 2 * r)];
    for y in 0..h {
        for x in 0..w {
            if cover[y * w + x] == 0 {
                continue;
            }
            for dy in -HALO..=HALO {
                for dx in -HALO..=HALO {
                    if dx * dx + dy * dy <= HALO_R2 {
                        let (hx, hy) = (
                            (x as i32 + HALO + dx) as usize,
                            (y as i32 + HALO + dy) as usize,
                        );
                        halo[hy * hw + hx] = 1;
                    }
                }
            }
        }
    }
    halo.into_boxed_slice()
}
