// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Decoded tiles kept in memory, and a canvas composed from them.
//!
//! Tiles are kept and composed as NV12 (crate::nv12), the video plane's
//! own format: what the video engine decodes JPEG into, and a byte and a
//! half a pixel, against two in RGB565. The engine decodes when there is
//! one; a tile it refuses, and every tile without it, decodes in software.

use std::path::PathBuf;

use crate::hw::{HwError, HwJpeg};
use crate::nv12;
use crate::tileset::TileSet;
use crate::view::{Layout, TILE};

const SIDE: usize = TILE as usize;

/// Bytes of an NV12 tile: the luma, then the chroma at half the height.
pub const TILE_BYTES: usize = SIDE * SIDE * 3 / 2;

/// What shows where there is no tile: the map's land colour.
pub const LAND: [u8; 3] = [242, 239, 233];

/// Tiles kept decoded: a canvas is 15, so a step in any direction or a
/// zoom back finds most of what it needs (96 KiB each).
pub const CACHED: usize = 36;

type Key = (u8, i64, i64);

struct Entry {
    key: Key,
    used: u64,
    /// The tile, NV12; None where the set has none (or it would not
    /// decode): not looked for again.
    pixels: Option<Box<[u8]>>,
}

/// The decoded tiles, least recently used out first.
pub struct Tiles {
    set: TileSet,
    hw: Option<HwJpeg>,
    entries: Vec<Entry>,
    clock: u64,
    path: PathBuf,
    file: Vec<u8>,
    /// A tile's row of land: luma, and Cb, Cr pairs.
    land_y: Box<[u8]>,
    land_c: Box<[u8]>,
    /// Decodes since the start: all, in software, and those that failed
    /// (for the log).
    pub decoded: u32,
    pub software: u32,
    pub failed: u32,
}

impl Tiles {
    /// `hw`: the video engine's decoder, for a set of JPEG tiles.
    pub fn new(set: TileSet, hw: Option<HwJpeg>) -> Self {
        let (y, cb, cr) = nv12::ycc(LAND);
        let hw = if set.is_jpeg() { hw } else { None };
        Self {
            set,
            hw,
            entries: Vec::with_capacity(CACHED),
            clock: 0,
            path: PathBuf::new(),
            file: Vec::new(),
            land_y: vec![y; SIDE].into_boxed_slice(),
            land_c: [cb, cr].repeat(SIDE / 2).into_boxed_slice(),
            decoded: 0,
            software: 0,
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
        let mut out = vec![0u8; TILE_BYTES].into_boxed_slice();
        if let Some(hw) = self.hw.as_mut() {
            match hw.decode(&self.file, &mut out) {
                Ok(()) => {
                    self.decoded += 1;
                    return Some(out);
                }
                Err(HwError::Refused) => {}
                Err(HwError::Io(e)) => log::warn!("map: tile {z}/{x}/{y} in hardware: {e}"),
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
        self.software += 1;
        nv12::from_rgb(&picture.rgb, SIDE, &mut out);
        Some(out)
    }

    /// Compose `layout` into `dst`, an NV12 canvas whose rows are `pitch`
    /// bytes apart and whose chroma begins at `chroma`: each tile kept or
    /// decoded, land where there is none. Written row by row, never read:
    /// `dst` may be write-combined memory.
    pub fn compose(&mut self, layout: &Layout, dst: &mut [u8], pitch: usize, chroma: usize) {
        let span = 1i64 << layout.zoom;
        // The middle first: it is what the panel shows.
        let mut order: Vec<(u32, u32)> = (0..layout.rows)
            .flat_map(|r| (0..layout.cols).map(move |c| (c, r)))
            .collect();
        let (mc, mr) = (layout.cols as i64 - 1, layout.rows as i64 - 1);
        order.sort_by_key(|&(c, r)| (2 * c as i64 - mc).abs() + (2 * r as i64 - mr).abs());
        let (luma, croma) = dst.split_at_mut(chroma.min(dst.len()));
        for (c, r) in order {
            let (x, y) = (layout.tx + i64::from(c), layout.ty + i64::from(r));
            let inside = (0..span).contains(&x) && (0..span).contains(&y);
            let slot = inside.then(|| self.slot((layout.zoom, x, y)));
            let pixels = slot.and_then(|i| self.entries[i].pixels.as_deref());
            let x0 = c as usize * SIDE;
            let (tile_y, tile_c) = match pixels {
                Some(p) => {
                    let (y, c) = p.split_at(SIDE * SIDE);
                    (Some(y), Some(c))
                }
                None => (None, None),
            };
            let r = r as usize;
            if !copy_rows(luma, pitch, x0, r * SIDE, SIDE, tile_y, &self.land_y)
                || !copy_rows(
                    croma,
                    pitch,
                    x0,
                    r * SIDE / 2,
                    SIDE / 2,
                    tile_c,
                    &self.land_c,
                )
            {
                return;
            }
        }
    }
}

/// `rows` rows of a tile's plane (`src`, rows `SIDE` bytes; `land`'s one
/// row repeated where there is none) to `dst` at byte `x0` of row `y0`.
/// False when `dst` does not hold them.
fn copy_rows(
    dst: &mut [u8],
    pitch: usize,
    x0: usize,
    y0: usize,
    rows: usize,
    src: Option<&[u8]>,
    land: &[u8],
) -> bool {
    for line in 0..rows {
        let start = (y0 + line) * pitch + x0;
        let Some(out) = dst.get_mut(start..start + SIDE) else {
            return false;
        };
        match src {
            Some(p) => out.copy_from_slice(&p[line * SIDE..(line + 1) * SIDE]),
            None => out.copy_from_slice(land),
        }
    }
    true
}
