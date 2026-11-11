// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Where the map looks, and where the panel's window sits on a canvas.
//!
//! Positions are world pixels at zoom 22 (Web Mercator, 256-pixel tiles):
//! a zoom z pixel is 2^(22 - z) of them, so every zoom's pixel is a whole
//! number of them and the arithmetic stays in integers.

/// The zoom of the world coordinates.
pub const WORLD_Z: u8 = 22;

/// Pixels a tile a side.
pub const TILE: i64 = 256;

/// What the map shows: the zoom and the point at the panel's centre.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub zoom: u8,
    pub x: i64,
    pub y: i64,
}

impl View {
    fn shift(&self) -> u8 {
        WORLD_Z - self.zoom.min(WORLD_Z)
    }

    /// Move the point shown by `dx`, `dy` pixels at this zoom (right and
    /// down positive), kept inside `bounds`.
    pub fn pan(&mut self, dx: i32, dy: i32, bounds: &[i64; 4]) {
        let s = self.shift();
        self.x += i64::from(dx) << s;
        self.y += i64::from(dy) << s;
        self.clamp(bounds);
    }

    /// One zoom level per step, in `min..=max`, about the same point.
    pub fn zoom_by(&mut self, step: i32, min: u8, max: u8) {
        let z = (i32::from(self.zoom) + step).clamp(i32::from(min), i32::from(max));
        self.zoom = z as u8;
    }

    pub fn clamp(&mut self, bounds: &[i64; 4]) {
        self.x = self.x.clamp(bounds[0], bounds[2]);
        self.y = self.y.clamp(bounds[1], bounds[3]);
    }

    /// The panel's top left in pixels at this zoom.
    fn top_left(&self, panel: (u32, u32)) -> (i64, i64) {
        let s = self.shift();
        (
            (self.x >> s) - i64::from(panel.0 / 2),
            (self.y >> s) - i64::from(panel.1 / 2),
        )
    }
}

/// What a canvas holds: tiles of `zoom` from (`tx`, `ty`), `cols` x `rows`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub zoom: u8,
    pub tx: i64,
    pub ty: i64,
    pub cols: u32,
    pub rows: u32,
}

impl Layout {
    /// The canvas around `view`, the panel's window in its middle: the
    /// most room to scroll before the next one.
    pub fn around(view: &View, panel: (u32, u32), cols: u32, rows: u32) -> Self {
        let (left, top) = view.top_left(panel);
        let spare_x = i64::from(cols) * TILE - i64::from(panel.0);
        let spare_y = i64::from(rows) * TILE - i64::from(panel.1);
        Self {
            zoom: view.zoom,
            // To the nearest tile: the window is then within half a tile
            // of the middle, and a canvas with a tile to spare each way
            // holds it.
            tx: (left - spare_x / 2 + TILE / 2).div_euclid(TILE),
            ty: (top - spare_y / 2 + TILE / 2).div_euclid(TILE),
            cols,
            rows,
        }
    }

    /// A point (world pixels at `WORLD_Z`) in sixteenths of this canvas's
    /// pixels.
    pub fn sixteenth(&self, (x, y): (i64, i64)) -> (i64, i64) {
        let s = i32::from(WORLD_Z - self.zoom.min(WORLD_Z)) - 4;
        let at = |v: i64| if s >= 0 { v >> s } else { v << -s };
        (at(x) - self.tx * TILE * 16, at(y) - self.ty * TILE * 16)
    }

    /// A point (world pixels at `WORLD_Z`) in this canvas's pixels.
    pub fn pixel(&self, (x, y): (i64, i64)) -> (i64, i64) {
        let s = WORLD_Z - self.zoom.min(WORLD_Z);
        ((x >> s) - self.tx * TILE, (y >> s) - self.ty * TILE)
    }

    /// Where the panel's window for `view` sits on this canvas, if it lies
    /// wholly inside.
    pub fn window(&self, view: &View, panel: (u32, u32)) -> Option<(u32, u32)> {
        if view.zoom != self.zoom {
            return None;
        }
        let (left, top) = view.top_left(panel);
        let x = left - self.tx * TILE;
        let y = top - self.ty * TILE;
        let fits_x = x >= 0 && x + i64::from(panel.0) <= i64::from(self.cols) * TILE;
        let fits_y = y >= 0 && y + i64::from(panel.1) <= i64::from(self.rows) * TILE;
        (fits_x && fits_y).then_some((x as u32, y as u32))
    }

    /// The canvas to have ready next: the one around the point where
    /// `view`, moving along `heading` (pixels at its zoom, any length),
    /// first leaves this canvas. None when it is not moving or not on
    /// this canvas.
    pub fn ahead(&self, view: &View, heading: (i64, i64), panel: (u32, u32)) -> Option<Self> {
        let (x, y) = self.window(view, panel)?;
        let spare_x = i64::from(self.cols) * TILE - i64::from(panel.0);
        let spare_y = i64::from(self.rows) * TILE - i64::from(panel.1);
        // How far each axis has to go to leave, a pixel past the edge.
        let out = |pos: u32, spare: i64, h: i64| match h.signum() {
            -1 => Some(i64::from(pos) + 1),
            1 => Some(spare - i64::from(pos) + 1),
            _ => None,
        };
        let (hx, hy) = heading;
        let along_x = out(x, spare_x, hx).map(|d| (d, hx.abs()));
        let along_y = out(y, spare_y, hy).map(|d| (d, hy.abs()));
        // The axis that leaves first: the least distance for its speed
        // (d1 / h1 < d2 / h2, without dividing).
        let (dx, dy) = match (along_x, along_y) {
            (None, None) => return None,
            (Some((d, _)), None) => (d * hx.signum(), 0),
            (None, Some((d, _))) => (0, d * hy.signum()),
            (Some((d1, h1)), Some((d2, h2))) => {
                if d1 * h2 <= d2 * h1 {
                    (d1 * hx.signum(), hy * d1 / h1)
                } else {
                    (hx * d2 / h2, d2 * hy.signum())
                }
            }
        };
        let s = view.shift();
        let past = View {
            zoom: view.zoom,
            x: view.x + (dx << s),
            y: view.y + (dy << s),
        };
        let next = Self::around(&past, panel, self.cols, self.rows);
        (next != *self).then_some(next)
    }
}

/// A latitude and longitude in degrees as world pixels at `WORLD_Z`
/// (Web Mercator); None outside the map's latitudes.
pub fn world_from_degrees(lat: f64, lon: f64) -> Option<(i64, i64)> {
    const MAX_LAT: f64 = 85.051_128_78;
    if !(-MAX_LAT..=MAX_LAT).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return None;
    }
    let size = (TILE as f64) * f64::from(1u32 << WORLD_Z);
    let s = lat.to_radians().sin();
    let x = (lon + 180.0) / 360.0 * size;
    let y = (0.5 - ((1.0 + s) / (1.0 - s)).ln() / (4.0 * std::f64::consts::PI)) * size;
    Some((x as i64, y as i64))
}

/// World pixels at `WORLD_Z` as a latitude and longitude in degrees.
pub fn degrees_from_world(x: i64, y: i64) -> (f64, f64) {
    let size = (TILE as f64) * f64::from(1u32 << WORLD_Z);
    let lon = x as f64 / size * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * y as f64 / size))
        .sinh()
        .atan()
        .to_degrees();
    (lat, lon)
}

/// A position as the settings give it, "latitude, longitude" in degrees,
/// as world pixels. Empty: none.
pub fn parse_position(text: &str) -> Result<Option<(i64, i64)>, String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let bad = || format!("{text:?}: latitude, longitude in degrees, e.g. \"49.6116, 6.1300\"");
    let (lat, lon) = text.split_once(',').ok_or_else(bad)?;
    let lat: f64 = lat.trim().parse().map_err(|_| bad())?;
    let lon: f64 = lon.trim().parse().map_err(|_| bad())?;
    world_from_degrees(lat, lon).map(Some).ok_or_else(bad)
}
