// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Decoded tiles kept in memory, and a canvas composed from them.

use std::path::PathBuf;

use crate::tileset::TileSet;
use crate::view::{Layout, TILE};

const SIDE: usize = TILE as usize;

/// What shows where there is no tile: the map's land colour.
pub const LAND: [u8; 3] = [242, 239, 233];

/// Tiles kept decoded: a canvas is 15, so a step in any direction or a
/// zoom back finds most of what it needs (128 KiB each in RGB565).
pub const CACHED: usize = 36;

type Key = (u8, i64, i64);

struct Entry {
    key: Key,
    used: u64,
    /// The tile in the canvas's pixel format; None where the set has none
    /// (or it would not decode): not looked for again.
    pixels: Option<Box<[u8]>>,
}

/// The decoded tiles, least recently used out first.
pub struct Tiles {
    set: TileSet,
    /// Bytes a pixel: 2 (RGB565) or 4 (XRGB8888).
    bpp: usize,
    entries: Vec<Entry>,
    clock: u64,
    path: PathBuf,
    file: Vec<u8>,
    land: Box<[u8]>,
    /// Decodes since the start, and the ones that failed (for the log).
    pub decoded: u32,
    pub failed: u32,
}

impl Tiles {
    pub fn new(set: TileSet, bpp: usize) -> Self {
        let mut land = vec![0u8; bpp];
        put(&mut land, bpp, LAND);
        Self {
            set,
            bpp,
            entries: Vec::with_capacity(CACHED),
            clock: 0,
            path: PathBuf::new(),
            file: Vec::new(),
            land: land.into_boxed_slice(),
            decoded: 0,
            failed: 0,
        }
    }

    /// The tile's slot in `entries`, decoded now if it is not kept.
    fn slot(&mut self, key: Key) -> usize {
        self.clock += 1;
        let at = match self.entries.iter().position(|e| e.key == key) {
            Some(i) => i,
            None => {
                let pixels = self.load(key);
                let entry = Entry {
                    key,
                    used: 0,
                    pixels,
                };
                if self.entries.len() < CACHED {
                    self.entries.push(entry);
                    self.entries.len() - 1
                } else {
                    let i = self
                        .entries
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, e)| e.used)
                        .map_or(0, |(i, _)| i);
                    self.entries[i] = entry;
                    i
                }
            }
        };
        self.entries[at].used = self.clock;
        at
    }

    fn load(&mut self, (z, x, y): Key) -> Option<Box<[u8]>> {
        match self.set.read(z, x, y, &mut self.path, &mut self.file) {
            Ok(true) => {}
            Ok(false) => return None,
            Err(e) => {
                self.failed += 1;
                log::warn!("map: tile {z}/{x}/{y}: {e}");
                return None;
            }
        }
        let picture = match lehu_image::decode(&self.file, TILE as u32, LAND) {
            Ok(p) if p.width as usize == SIDE && p.height as usize == SIDE => p,
            Ok(p) => {
                self.failed += 1;
                log::warn!("map: tile {z}/{x}/{y} is {}x{}", p.width, p.height);
                return None;
            }
            Err(e) => {
                self.failed += 1;
                log::warn!("map: tile {z}/{x}/{y}: {e}");
                return None;
            }
        };
        self.decoded += 1;
        let mut out = vec![0u8; SIDE * SIDE * self.bpp];
        let (rgb, _) = picture.rgb.as_chunks::<3>();
        for (dst, &px) in out.chunks_exact_mut(self.bpp).zip(rgb) {
            put(dst, self.bpp, px);
        }
        Some(out.into_boxed_slice())
    }

    /// Compose `layout` into `dst`, whose rows are `pitch` bytes apart:
    /// each tile kept or decoded, land where there is none. Written row by
    /// row, never read: `dst` may be write-combined memory.
    pub fn compose(&mut self, layout: &Layout, dst: &mut [u8], pitch: usize) {
        let bpp = self.bpp;
        let row = SIDE * bpp;
        let span = 1i64 << layout.zoom;
        // The middle first: it is what the panel shows.
        let mut order: Vec<(u32, u32)> = (0..layout.rows)
            .flat_map(|r| (0..layout.cols).map(move |c| (c, r)))
            .collect();
        let (mc, mr) = (layout.cols as i64 - 1, layout.rows as i64 - 1);
        order.sort_by_key(|&(c, r)| (2 * c as i64 - mc).abs() + (2 * r as i64 - mr).abs());
        for (c, r) in order {
            let (x, y) = (layout.tx + i64::from(c), layout.ty + i64::from(r));
            let inside = (0..span).contains(&x) && (0..span).contains(&y);
            let x0 = c as usize * row;
            let y0 = r as usize * SIDE;
            let slot = inside.then(|| self.slot((layout.zoom, x, y)));
            let pixels = slot.and_then(|i| self.entries[i].pixels.as_deref());
            for line in 0..SIDE {
                let start = (y0 + line) * pitch + x0;
                let Some(out) = dst.get_mut(start..start + row) else {
                    return;
                };
                match pixels {
                    Some(p) => out.copy_from_slice(&p[line * row..(line + 1) * row]),
                    None => {
                        for px in out.chunks_exact_mut(bpp) {
                            px.copy_from_slice(&self.land);
                        }
                    }
                }
            }
        }
    }
}

/// One pixel in the canvas's format, little-endian as DRM lays it out.
fn put(dst: &mut [u8], bpp: usize, [r, g, b]: [u8; 3]) {
    if bpp == 2 {
        let v = (u16::from(r >> 3) << 11) | (u16::from(g >> 2) << 5) | u16::from(b >> 3);
        dst.copy_from_slice(&v.to_le_bytes());
    } else {
        dst.copy_from_slice(&[b, g, r, 0xff]);
    }
}
