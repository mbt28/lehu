// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Decoded tiles kept in memory, and a canvas composed from them.
//!
//! Tiles are kept and composed as NV12 (crate::nv12), the video plane's
//! own format: what the video engine decodes JPEG into, and a byte and a
//! half a pixel, against two in RGB565. The engine decodes when there is
//! one; a tile it refuses, and every tile without it, decodes in software.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::hw::{HwError, HwJpeg};
use crate::nv12;
use crate::tileset::TileSet;
use crate::view::{Layout, TILE};

const SIDE: usize = TILE as usize;

/// Bytes of an NV12 tile: the luma, then the chroma at half the height.
pub const TILE_BYTES: usize = SIDE * SIDE * 3 / 2;

/// What shows where there is no tile, unless the set says otherwise: the
/// map's land colour.
pub const LAND: [u8; 3] = [242, 239, 233];

/// Tiles kept decoded (96 KiB each, taken as they are needed): a canvas
/// is 15; a turned frame two zooms past the set's last wants up to 36, and
/// the tiles a zoom and two zooms out they are enlarged from, all at once;
/// and those decoded ahead along the route (crate::viewer), up to 32, so
/// that they do not push the frame's own out. Fewer, and they push each
/// other out while one frame is drawn.
pub const CACHED: usize = 96;

/// Buffers of tiles let go kept for the next decoded, at most.
const SPARE: usize = 8;

/// Tiles kept a second time with their rows and columns swapped, at most
/// (`Tiles::grid_for`): a turned frame's own, up to 36, and a few left
/// behind it. More only while one frame wants more.
const SWAPPED: usize = 40;

/// Swapped tiles made for a frame that does not need them yet, at most: a
/// fifth of the frame's own time, until they are all there.
const SWAPPED_AHEAD: usize = 2;

/// The swapped tiles are let go when no frame wanted them for this long.
const SWAPPED_IDLE: Duration = Duration::from_secs(30);

/// Tiles remembered as missing, besides: a few bytes each, kept apart so
/// that a trip over empty land does not push decoded tiles out.
const MISSING: usize = 256;

/// A missing tile is made from the one a zoom out, enlarged, and that one
/// from the next, up to this many zooms: past that it is too soft to be
/// worth it.
const MAX_ENLARGE: u8 = 4;

type Key = (u8, i64, i64);

struct Entry {
    key: Key,
    used: u64,
    /// The tile, NV12; None where the set has none (or it would not
    /// decode): not looked for again.
    pixels: Option<Box<[u8]>>,
    /// How many times enlarged: 0 for a tile of the set.
    soft: u8,
    /// The tile again, its rows and columns swapped (`nv12::swap_tile`),
    /// while turned frames want it.
    swapped: Option<Box<[u8]>>,
}

/// The decoded tiles, least recently used out first.
pub struct Tiles {
    set: TileSet,
    /// The other set, day or night, when there is one: `set_night` swaps.
    other: Option<TileSet>,
    night: bool,
    /// Night made from the day tiles, while it is night and there is no
    /// night set.
    lut: Option<nv12::Night>,
    hw: Option<HwJpeg>,
    entries: Vec<Entry>,
    /// The buffers of tiles let go, for the next decoded: a tile's 96 KiB
    /// is above the allocator's mmap threshold, so a fresh one costs a
    /// mapping and its page faults (half a millisecond) each time.
    spare: Vec<Box<[u8]>>,
    clock: u64,
    path: PathBuf,
    file: Vec<u8>,
    /// A tile's row of land: luma, and Cb, Cr pairs.
    land_y: Box<[u8]>,
    land_c: Box<[u8]>,
    /// Decodes since the start: all, in software, and those that failed;
    /// and the missing tiles made by enlarging (for the log).
    pub decoded: u32,
    pub software: u32,
    pub failed: u32,
    pub enlarged: u32,
    /// Tiles swapped since the start, and frames turned from them.
    pub swaps: u32,
    pub swapped_frames: u32,
    /// When a frame last wanted swapped tiles, or nearly did.
    swap_wanted: Option<Instant>,
    /// How many enlargements deep `load` is now.
    depth: u8,
    /// Every tile a test picture instead (`LEHU_RANGE_TEST`): a grey ramp
    /// and colour bars, at full range (true) or limited (false), to check
    /// the display's conversion by eye.
    test_tile: Option<bool>,
}

impl Tiles {
    /// `hw`: the video engine's decoder, for a set of JPEG tiles.
    pub fn new(set: TileSet, night: Option<TileSet>, hw: Option<HwJpeg>) -> Self {
        let (y, cb, cr) = nv12::ycc(set.land);
        let hw = if set.is_jpeg() { hw } else { None };
        Self {
            set,
            other: night,
            night: false,
            lut: None,
            hw,
            entries: Vec::with_capacity(CACHED + MISSING),
            spare: Vec::new(),
            clock: 0,
            path: PathBuf::new(),
            file: Vec::new(),
            land_y: vec![y; SIDE].into_boxed_slice(),
            land_c: [cb, cr].repeat(SIDE / 2).into_boxed_slice(),
            decoded: 0,
            software: 0,
            failed: 0,
            enlarged: 0,
            swaps: 0,
            swapped_frames: 0,
            swap_wanted: None,
            depth: 0,
            test_tile: None,
        }
    }

    /// Every tile the test picture (`LEHU_RANGE_TEST`), full range or
    /// limited.
    pub fn set_test_tile(&mut self, full: Option<bool>) {
        self.test_tile = full;
        self.entries.clear();
    }

    /// The tile's slot in `entries`, decoded now if it is not kept.
    fn slot(&mut self, key: Key) -> usize {
        self.clock += 1;
        let at = match self.entries.iter().position(|e| e.key == key) {
            Some(i) => i,
            None => {
                let (pixels, soft) = match self.load(key) {
                    Some((p, soft)) => (Some(p), soft),
                    None => (None, 0),
                };
                let entry = Entry {
                    key,
                    used: 0,
                    pixels,
                    soft,
                    swapped: None,
                };
                let kept = self.entries.iter().filter(|e| e.pixels.is_some()).count();
                let full = if entry.pixels.is_some() {
                    kept >= CACHED
                } else {
                    self.entries.len() - kept >= MISSING
                };
                if full {
                    // The least recently used of its own kind makes way.
                    let has = entry.pixels.is_some();
                    let i = self
                        .entries
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| e.pixels.is_some() == has)
                        .min_by_key(|(_, e)| e.used)
                        .map_or(0, |(i, _)| i);
                    let old = std::mem::replace(&mut self.entries[i], entry);
                    for b in [old.pixels, old.swapped].into_iter().flatten() {
                        self.spared(b);
                    }
                    i
                } else {
                    self.entries.push(entry);
                    self.entries.len() - 1
                }
            }
        };
        self.entries[at].used = self.clock;
        at
    }

    /// Show the night tiles, or the day's; false when that changes
    /// nothing (already so). Without night tiles, night is made from the
    /// day's ([`nv12::Night`]). Every tile kept is let go.
    pub fn set_night(&mut self, night: bool) -> bool {
        if night == self.night {
            return false;
        }
        if let Some(other) = self.other.take() {
            self.other = Some(std::mem::replace(&mut self.set, other));
        } else {
            self.lut = night.then(|| nv12::Night::new(self.set.land));
        }
        self.night = night;
        self.entries.clear();
        let (y, cb, cr) = self.land();
        self.land_y = vec![y; SIDE].into_boxed_slice();
        self.land_c = [cb, cr].repeat(SIDE / 2).into_boxed_slice();
        true
    }

    /// The colour where there is no tile, as it shows now.
    fn land(&self) -> (u8, u8, u8) {
        let land = nv12::ycc(self.set.land);
        self.lut.as_ref().map_or(land, |l| l.ycc(land))
    }

    /// The night tiles are shown.
    pub fn night(&self) -> bool {
        self.night
    }

    /// The tile, and how many times it was enlarged.
    fn load(&mut self, (z, x, y): Key) -> Option<(Box<[u8]>, u8)> {
        if let Some(full) = self.test_tile {
            let mut out = self.buffer();
            nv12::test_picture(&mut out, SIDE, full);
            return Some((out, 0));
        }
        match self.set.read(z, x, y, &mut self.path, &mut self.file) {
            Ok(true) => {}
            Ok(false) => return self.enlarge((z, x, y)),
            Err(e) => {
                self.failed += 1;
                log::warn!("map: tile {z}/{x}/{y}: {e}");
                return None;
            }
        }
        let mut out = self.buffer();
        if let Some(hw) = self.hw.as_mut() {
            match hw.decode(&self.file, &mut out) {
                Ok(()) => {
                    self.decoded += 1;
                    if let Some(lut) = &self.lut {
                        lut.apply(&mut out, SIDE);
                    }
                    return Some((out, 0));
                }
                Err(HwError::Refused) => {}
                Err(HwError::Io(e)) => log::warn!("map: tile {z}/{x}/{y} in hardware: {e}"),
            }
        }
        let picture = match lehu_image::decode(&self.file, TILE as u32, self.set.land) {
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
        if let Some(lut) = &self.lut {
            lut.apply(&mut out, SIDE);
        }
        Some((out, 0))
    }

    /// A tile's buffer: one let go, else a fresh one. Every byte is written
    /// by whoever fills it (the engine's untile, the software decode, the
    /// enlarging), so a used one needs no clearing.
    fn buffer(&mut self) -> Box<[u8]> {
        self.spare
            .pop()
            .unwrap_or_else(|| vec![0u8; TILE_BYTES].into_boxed_slice())
    }

    /// A buffer let go, kept for the next tile if there is room.
    fn spared(&mut self, buffer: Box<[u8]>) {
        if self.spare.len() < SPARE {
            self.spare.push(buffer);
        }
    }

    /// Tile `key` is kept decoded (or known missing).
    pub fn kept(&self, key: (u8, i64, i64)) -> bool {
        self.entries.iter().any(|e| e.key == key)
    }

    /// Tile `key` decoded now if it is not kept, for a frame to come: true
    /// when something was done.
    pub fn prefetch(&mut self, key: (u8, i64, i64)) -> bool {
        if !inside(key.0, key.1, key.2) || self.kept(key) {
            return false;
        }
        self.slot(key);
        true
    }

    /// A tile the set lacks, as its quarter of the tile a zoom out,
    /// enlarged (that one made the same way if it is missing too): softer,
    /// but the map rather than bare land, so a set can stop at a lower
    /// zoom than the map shows.
    fn enlarge(&mut self, (z, x, y): Key) -> Option<(Box<[u8]>, u8)> {
        if z <= self.set.min_zoom || self.depth >= MAX_ENLARGE {
            return None;
        }
        self.depth += 1;
        let i = self.slot((z - 1, x >> 1, y >> 1));
        self.depth -= 1;
        let parent = &self.entries[i];
        // A tile kept from an earlier enlargement counts its own.
        let soft = parent.soft + 1;
        if soft > MAX_ENLARGE || parent.pixels.is_none() {
            return None;
        }
        let mut out = self.buffer();
        let src = self.entries[i].pixels.as_deref()?;
        let quarter = ((x & 1) as usize, (y & 1) as usize);
        nv12::enlarge_quarter(src, SIDE, quarter, &mut out);
        self.enlarged += 1;
        Some((out, soft))
    }

    /// Decode one tile of `layout` that is not kept yet, for a canvas
    /// composed later; false when there is none left. One at a time, so
    /// the caller can look for something more urgent between them.
    pub fn prepare(&mut self, layout: &Layout) -> bool {
        let z = layout.zoom;
        let next = order(layout).into_iter().find_map(|(c, r)| {
            let (x, y) = (layout.tx + i64::from(c), layout.ty + i64::from(r));
            let kept = self.entries.iter().any(|e| e.key == (z, x, y));
            (inside(z, x, y) && !kept).then_some((z, x, y))
        });
        match next {
            Some(key) => {
                self.slot(key);
                true
            }
            None => false,
        }
    }

    /// Compose `layout` into `dst`, an NV12 canvas whose rows are `pitch`
    /// bytes apart and whose chroma begins at `chroma`: each tile kept or
    /// decoded, land where there is none. Written row by row, never read:
    /// `dst` may be write-combined memory.
    pub fn compose(&mut self, layout: &Layout, dst: &mut [u8], pitch: usize, chroma: usize) {
        // A canvas is north up: no frame is turned now.
        self.drop_swapped();
        let (luma, croma) = dst.split_at_mut(chroma.min(dst.len()));
        for (c, r) in order(layout) {
            let (x, y) = (layout.tx + i64::from(c), layout.ty + i64::from(r));
            let slot = inside(layout.zoom, x, y).then(|| self.slot((layout.zoom, x, y)));
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

impl Tiles {
    /// The tiles of `zoom` from (`tx`, `ty`), `cols` x `rows`, decoded
    /// where they are not kept, for a rotated frame: as they are, at
    /// whatever angle (the tests' measure of `grid_for`).
    #[cfg(test)]
    pub fn grid(
        &mut self,
        zoom: u8,
        (tx, ty): (i64, i64),
        cols: usize,
        rows: usize,
    ) -> nv12::Grid<'_> {
        let slots = self.held(zoom, (tx, ty), cols, rows);
        self.upright(&slots, cols, rows)
    }

    /// `grid` for a frame whose pixels step `across` the tiles: laid out
    /// swapped, of swapped tiles, when the step runs down the tiles' rows
    /// more than along them. The picture is the same either way, the
    /// frame a third quicker turned (`nv12::block_for`).
    ///
    /// A tile is swapped when a frame first wants it so (2.7 ms on the
    /// F1C200s), and a few ahead of that once the step is past 27 degrees
    /// from the rows, so that a map turning onto its side has them all
    /// when it gets there. Let go when frames have been upright for a
    /// while.
    pub fn grid_for(
        &mut self,
        zoom: u8,
        (tx, ty): (i64, i64),
        cols: usize,
        rows: usize,
        across: (i64, i64),
    ) -> nv12::Grid<'_> {
        let before = self.clock;
        let slots = self.held(zoom, (tx, ty), cols, rows);
        let (along, down) = (across.0.abs(), across.1.abs());
        let (steep, near) = (down > along, 2 * down > along);
        if !near {
            if self
                .swap_wanted
                .is_some_and(|t| t.elapsed() >= SWAPPED_IDLE)
            {
                self.drop_swapped();
            }
            return self.upright(&slots, cols, rows);
        }
        self.swap_wanted = Some(Instant::now());
        let mut left = if steep { usize::MAX } else { SWAPPED_AHEAD };
        for &i in slots.iter().flatten() {
            if left == 0 {
                break;
            }
            if self.swap(i) {
                left -= 1;
            }
        }
        // Over the limit: those no frame used the longest go, this
        // frame's own never.
        let mut kept = self.entries.iter().filter(|e| e.swapped.is_some()).count();
        while kept > SWAPPED {
            let oldest = self
                .entries
                .iter()
                .enumerate()
                .filter(|(_, e)| e.swapped.is_some() && e.used <= before)
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i);
            let Some(i) = oldest else {
                break;
            };
            if let Some(b) = self.entries[i].swapped.take() {
                self.spared(b);
            }
            kept -= 1;
        }
        if !steep {
            return self.upright(&slots, cols, rows);
        }
        self.swapped_frames += 1;
        let (y, cb, cr) = self.land();
        // Row by row of the swapped grid: `rows` across, `cols` down.
        let tiles = (0..cols)
            .flat_map(|c| (0..rows).map(move |r| r * cols + c))
            .map(|at| slots[at].and_then(|i| self.entries[i].swapped.as_deref()))
            .collect();
        nv12::Grid {
            tiles,
            cols: rows,
            rows: cols,
            side: SIDE,
            land: (y, cb, cr),
            swapped: true,
        }
    }

    /// Tile `key` swapped too, if frames wanted swapped tiles of late and
    /// there is room: for a tile decoded ahead (`prefetch`). True when
    /// something was done.
    pub fn swap_ahead(&mut self, key: (u8, i64, i64)) -> bool {
        if self.swap_wanted.is_none_or(|t| t.elapsed() >= SWAPPED_IDLE)
            || self.entries.iter().filter(|e| e.swapped.is_some()).count() >= SWAPPED
        {
            return false;
        }
        match self.entries.iter().position(|e| e.key == key) {
            Some(i) => self.swap(i),
            None => false,
        }
    }

    /// The tile of slot `i` swapped too, if it is not: true when it was
    /// made now.
    fn swap(&mut self, i: usize) -> bool {
        if self.entries[i].swapped.is_some() || self.entries[i].pixels.is_none() {
            return false;
        }
        let mut out = self.buffer();
        if let Some(src) = self.entries[i].pixels.as_deref() {
            nv12::swap_tile(src, &mut out);
        }
        self.entries[i].swapped = Some(out);
        self.swaps += 1;
        true
    }

    /// Every swapped tile let go.
    fn drop_swapped(&mut self) {
        if self.swap_wanted.take().is_none() {
            return;
        }
        for i in 0..self.entries.len() {
            if let Some(b) = self.entries[i].swapped.take() {
                self.spared(b);
            }
        }
    }

    /// The slots of the tiles of `zoom` from (`tx`, `ty`), `cols` x
    /// `rows`, row by row, each decoded if it is not kept; None outside
    /// the world.
    fn held(
        &mut self,
        zoom: u8,
        (tx, ty): (i64, i64),
        cols: usize,
        rows: usize,
    ) -> Vec<Option<usize>> {
        let keys: Vec<Option<Key>> = (0..rows as i64)
            .flat_map(|r| (0..cols as i64).map(move |c| (tx + c, ty + r)))
            .map(|(x, y)| inside(zoom, x, y).then_some((zoom, x, y)))
            .collect();
        // A tile taken in can push out one taken in before it when a frame
        // wants more than the cache keeps (enlarging takes the tiles a zoom
        // out in too): look again until every one is there.
        let mut slots = vec![None; keys.len()];
        for _ in 0..3 {
            for (slot, key) in slots.iter_mut().zip(&keys) {
                if let Some(k) = key {
                    *slot = Some(self.slot(*k));
                }
            }
            let held = slots.iter().zip(&keys).all(|(s, k)| match (s, k) {
                (Some(i), Some(k)) => self.entries[*i].key == *k,
                _ => true,
            });
            if held {
                break;
            }
        }
        slots
    }

    /// The grid of the tiles in `slots`, as they are.
    fn upright(&self, slots: &[Option<usize>], cols: usize, rows: usize) -> nv12::Grid<'_> {
        let (y, cb, cr) = self.land();
        nv12::Grid {
            tiles: slots
                .iter()
                .map(|s| s.and_then(|i| self.entries[i].pixels.as_deref()))
                .collect(),
            cols,
            rows,
            side: SIDE,
            land: (y, cb, cr),
            swapped: false,
        }
    }

    /// Compose only the part `(left, top, right, bottom)` of `layout`'s
    /// canvas (its pixels; clipped to it, widened to even), as `compose`
    /// does the whole: to take a marker off again.
    pub fn compose_rect(
        &mut self,
        layout: &Layout,
        dst: &mut [u8],
        pitch: usize,
        chroma: usize,
        (left, top, right, bottom): (i64, i64, i64, i64),
    ) {
        let side = SIDE as i64;
        let (w, h) = (i64::from(layout.cols) * side, i64::from(layout.rows) * side);
        let (x0, y0) = (left.max(0) & !1, top.max(0) & !1);
        let (x1, y1) = ((right.min(w) + 1) & !1, (bottom.min(h) + 1) & !1);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (luma, croma) = dst.split_at_mut(chroma.min(dst.len()));
        for r in (y0 / side)..=((y1 - 1) / side) {
            for c in (x0 / side)..=((x1 - 1) / side) {
                let (tx, ty) = (layout.tx + c, layout.ty + r);
                let slot = inside(layout.zoom, tx, ty).then(|| self.slot((layout.zoom, tx, ty)));
                let pixels = slot.and_then(|i| self.entries[i].pixels.as_deref());
                // The part of this tile inside the rectangle, in its pixels.
                let (ax, bx) = (
                    (x0 - c * side).max(0) as usize,
                    (x1 - c * side).min(side) as usize,
                );
                let (ay, by) = (
                    (y0 - r * side).max(0) as usize,
                    (y1 - r * side).min(side) as usize,
                );
                let at = |row: usize| row * pitch + c as usize * SIDE + ax;
                for y in ay..by {
                    let o = at(r as usize * SIDE + y);
                    let Some(out) = luma.get_mut(o..o + (bx - ax)) else {
                        return;
                    };
                    match pixels {
                        Some(p) => out.copy_from_slice(&p[y * SIDE + ax..y * SIDE + bx]),
                        None => out.copy_from_slice(&self.land_y[ax..bx]),
                    }
                }
                for y in (ay / 2)..(by / 2) {
                    let o = at(r as usize * SIDE / 2 + y);
                    let Some(out) = croma.get_mut(o..o + (bx - ax)) else {
                        return;
                    };
                    match pixels {
                        Some(p) => {
                            let p = &p[SIDE * SIDE..];
                            out.copy_from_slice(&p[y * SIDE + ax..y * SIDE + bx]);
                        }
                        None => out.copy_from_slice(&self.land_c[ax..bx]),
                    }
                }
            }
        }
    }
}

/// A layout's tiles, column and row, the middle first: what the panel
/// shows.
fn order(layout: &Layout) -> Vec<(u32, u32)> {
    let mut order: Vec<(u32, u32)> = (0..layout.rows)
        .flat_map(|r| (0..layout.cols).map(move |c| (c, r)))
        .collect();
    let (mc, mr) = (layout.cols as i64 - 1, layout.rows as i64 - 1);
    order.sort_by_key(|&(c, r)| (2 * c as i64 - mc).abs() + (2 * r as i64 - mr).abs());
    order
}

/// Tile (`x`, `y`) is inside the world at zoom `z`.
fn inside(z: u8, x: i64, y: i64) -> bool {
    let span = 1i64 << z;
    (0..span).contains(&x) && (0..span).contains(&y)
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
