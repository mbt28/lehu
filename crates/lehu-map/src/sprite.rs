// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The marker on a plane of its own, above the map: the map then moves
//! under it by a commit alone, never redrawn for it.
//!
//! On the cursor plane when the display has one and it is wanted: a
//! square the hardware draws over everything, 32 pixels with 15 colours
//! (small) or 64 with one (medium, large: dark blue by day, white at
//! night). Two of them, a new direction drawn into the one the cursor does
//! not hold, since the driver reads a picture only when it is handed a new
//! one. Else on the sprite plane: one ARGB8888 canvas holds the dot and the
//! arrow in 64 directions, each drawn the first time it is wanted; the
//! plane shows one cell of it.

use lehu_drm::{CURSOR_SIZE, Canvas, Display, Sprite};

/// The arrow's directions, clockwise from up.
const DIRECTIONS: usize = 64;
/// Cells a row of the sprite canvas: the arrows, then the dot.
const COLS: usize = 9;
const ROWS: usize = (DIRECTIONS + 1).div_ceil(COLS);

/// The marker's colours (R, G, B), as `nv12::dot` and `nv12::arrow` have
/// them.
const FILL: [u8; 3] = [26, 115, 232];
const EDGE: [u8; 3] = [255, 255, 255];
/// The one colour of the 64 pixel cursor, by day and at night: it shows
/// on the light map, on the blue route, and on the dark map.
const ONE_DAY: [u8; 3] = [20, 40, 90];
const ONE_NIGHT: [u8; 3] = [255, 255, 255];

/// Where the position arrow goes: a plane of its own, or the display's
/// hardware cursor at a size (`map.cursor`, `map.cursor-size`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MarkerPlane {
    /// Its own plane, in its colours at its own size.
    Sprite,
    /// The cursor: 32 pixels, in its colours.
    CursorSmall,
    /// The cursor: 64 pixels, its own size, in one colour.
    #[default]
    CursorMedium,
    /// The cursor: 64 pixels, a third larger, in one colour.
    CursorLarge,
}

/// How a look's pixels are made.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Paint {
    /// Blended and straight: the sprite plane's.
    Straight,
    /// Premultiplied, in at most 14 colours: the 32 pixel cursor's.
    Palette,
    /// Opaque or transparent, in one colour by day and one at night: the
    /// 64 pixel cursor's.
    One,
}

/// A marker's size and shape, in pixels.
struct Look {
    /// A cell a side; the marker's middle is at its middle.
    cell: u32,
    dot_r: f64,
    ring_r: f64,
    /// The arrow's fill to its tip, and its white edge's.
    arrow_r: f64,
    edge_r: f64,
    /// The back corners, behind the middle and to the side, and the
    /// notch between them, in radii.
    back: f64,
    wide: f64,
    notch: f64,
    paint: Paint,
}

/// On the sprite plane, as the map draws it itself.
const SHEET: Look = Look {
    cell: 48,
    dot_r: 9.0,
    ring_r: 13.0,
    arrow_r: 15.0,
    edge_r: 15.0 + 3.0 * 1.6,
    back: 0.8,
    wide: 0.75,
    notch: 0.35,
    paint: Paint::Straight,
};

/// On the small cursor: the corners on the circle the cell holds, so it
/// turns every way inside 32 pixels, three quarters of the sheet's length.
const SMALL: Look = Look {
    cell: CURSOR_SIZE,
    dot_r: 9.0,
    ring_r: 13.0,
    arrow_r: 13.1,
    edge_r: 15.5,
    back: 0.70,
    wide: 0.70,
    notch: 0.28,
    paint: Paint::Palette,
};

/// On the 64 pixel cursor at the sheet's size: its outline, in one colour.
const MEDIUM: Look = Look {
    cell: 64,
    dot_r: 13.0,
    ring_r: 13.0,
    arrow_r: SHEET.edge_r,
    edge_r: SHEET.edge_r,
    back: SHEET.back,
    wide: SHEET.wide,
    notch: SHEET.notch,
    paint: Paint::One,
};

/// A third larger, its back corners near the circle the cell holds.
const LARGE: Look = Look {
    cell: 64,
    dot_r: 17.0,
    ring_r: 17.0,
    arrow_r: 27.0,
    edge_r: 27.0,
    ..MEDIUM
};

pub struct Sprites {
    kind: Kind,
    /// The one-colour cursor's colour is the night's.
    night: bool,
}

enum Kind {
    Sheet {
        canvas: Canvas,
        drawn: [bool; DIRECTIONS + 1],
    },
    /// The look, two canvases and the cell each holds, by day or night.
    Cursor {
        look: &'static Look,
        bufs: Vec<(Canvas, Option<(usize, bool)>)>,
    },
}

impl Sprites {
    /// On the cursor at the size `plane` asks for, when there is one (the
    /// small one where the cursor takes no 64 pixel square), else the
    /// sprite plane; None with neither, or no memory for them.
    pub fn new(display: &Display, plane: MarkerPlane) -> Option<Self> {
        let look = match plane {
            MarkerPlane::Sprite => None,
            MarkerPlane::CursorSmall => Some(&SMALL),
            MarkerPlane::CursorMedium => Some(&MEDIUM),
            MarkerPlane::CursorLarge => Some(&LARGE),
        };
        if let Some(mut look) = look.filter(|_| display.has_cursor()) {
            if look.cell > display.cursor_max() {
                log::info!(
                    "map: the cursor takes no {} pixel square; the small marker",
                    look.cell
                );
                look = &SMALL;
            }
            let two: Result<Vec<_>, _> = (0..2).map(|_| display.cursor_canvas(look.cell)).collect();
            match two {
                Ok(c) => {
                    return Some(Self {
                        kind: Kind::Cursor {
                            look,
                            bufs: c.into_iter().map(|c| (c, None)).collect(),
                        },
                        night: false,
                    });
                }
                Err(e) => log::warn!("map: no memory for the marker's cursor ({e})"),
            }
        }
        if !display.has_sprite() {
            return None;
        }
        let (w, h) = (COLS as u32 * SHEET.cell, ROWS as u32 * SHEET.cell);
        match display.sprite_canvas(w, h) {
            Ok(canvas) => Some(Self {
                kind: Kind::Sheet {
                    canvas,
                    drawn: [false; DIRECTIONS + 1],
                },
                night: false,
            }),
            Err(e) => {
                log::warn!("map: no memory for the marker's sprite ({e}); drawn into the map");
                None
            }
        }
    }

    /// Day or night: the one-colour cursor is drawn in the night's colour
    /// from the next `at` on.
    pub fn set_night(&mut self, night: bool) {
        self.night = night;
    }

    /// Where the marker is, for the log.
    pub fn plane(&self) -> String {
        match &self.kind {
            Kind::Sheet { .. } => "the sprite plane".into(),
            Kind::Cursor { look, .. } => format!("the cursor plane, {0}x{0}", look.cell),
        }
    }

    /// The display still takes the marker on its plane: one can be
    /// refused at its first commit.
    pub fn usable(&self, display: &Display) -> bool {
        match self.kind {
            Kind::Sheet { .. } => display.has_sprite(),
            Kind::Cursor { .. } => display.has_cursor(),
        }
    }

    /// The marker centred at (`x`, `y`) of the panel: the arrow along
    /// `heading` (x right, y down, any length), or the dot for None or
    /// (0, 0).
    pub fn at(
        &mut self,
        display: &Display,
        heading: Option<(i64, i64)>,
        (x, y): (i64, i64),
    ) -> Sprite<'_> {
        let night = self.night;
        let cell = match heading {
            Some((hx, hy)) if (hx, hy) != (0, 0) => direction(hx, hy),
            _ => DIRECTIONS,
        };
        let place =
            |v: i64, look: &Look| (v - i64::from(look.cell / 2)).clamp(-(1 << 20), 1 << 20) as i32;
        match &mut self.kind {
            Kind::Sheet { canvas, drawn } => {
                if !drawn[cell] {
                    let (x0, y0) = (
                        (cell % COLS) * SHEET.cell as usize,
                        (cell / COLS) * SHEET.cell as usize,
                    );
                    let pitch = canvas.pitch();
                    draw(canvas.pixels(), pitch, (x0, y0), &SHEET, cell, false);
                    drawn[cell] = true;
                }
                Sprite {
                    canvas,
                    sx: (cell % COLS) as u32 * SHEET.cell,
                    sy: (cell / COLS) as u32 * SHEET.cell,
                    w: SHEET.cell,
                    h: SHEET.cell,
                    x: place(x, &SHEET),
                    y: place(y, &SHEET),
                }
            }
            Kind::Cursor { look, bufs } => {
                // Only the one-colour cursor changes at night.
                let key = (cell, night && look.paint == Paint::One);
                let i = match bufs.iter().position(|b| b.1 == Some(key)) {
                    Some(i) => i,
                    None => {
                        // Never the one the cursor holds: the driver took
                        // its pixels already, and keeps them.
                        let i = bufs
                            .iter()
                            .position(|b| !display.cursor_holds(&b.0))
                            .unwrap_or(0);
                        let (canvas, holds) = &mut bufs[i];
                        let pitch = canvas.pitch();
                        draw(canvas.pixels(), pitch, (0, 0), look, key.0, key.1);
                        *holds = Some(key);
                        i
                    }
                };
                Sprite {
                    canvas: &bufs[i].0,
                    sx: 0,
                    sy: 0,
                    w: look.cell,
                    h: look.cell,
                    x: place(x, look),
                    y: place(y, look),
                }
            }
        }
    }

    pub fn into_canvases(self) -> Vec<Canvas> {
        match self.kind {
            Kind::Sheet { canvas, .. } => vec![canvas],
            Kind::Cursor { bufs, .. } => bufs.into_iter().map(|(c, _)| c).collect(),
        }
    }
}

/// Draw `cell` (a direction, or `DIRECTIONS` for the dot) as `look` has
/// it at (`x0`, `y0`) of `dst`, rows `pitch` bytes apart, 16 samples a
/// pixel for smooth edges (the one-colour cursor: covered by half or more);
/// `night` picks its colour. The corners in floating point, the samples in
/// integers (1/64 px): no FPU here.
fn draw(
    dst: &mut [u8],
    pitch: usize,
    (x0, y0): (usize, usize),
    look: &Look,
    cell: usize,
    night: bool,
) {
    let one = if night { ONE_NIGHT } else { ONE_DAY };
    const UNIT: f64 = 64.0;
    let size = look.cell as usize;
    let mid = f64::from(look.cell) / 2.0;
    let arrow = |r: f64| {
        let t = cell as f64 * std::f64::consts::TAU / DIRECTIONS as f64;
        let (ux, uy) = (t.sin(), -t.cos());
        let at = |along: f64, across: f64| {
            (
                ((mid + ux * along - uy * across) * UNIT) as i32,
                ((mid + uy * along + ux * across) * UNIT) as i32,
            )
        };
        let tip = at(r, 0.0);
        let left = at(-look.back * r, -look.wide * r);
        let right = at(-look.back * r, look.wide * r);
        let notch = at(-look.notch * r, 0.0);
        [[tip, left, notch], [tip, notch, right]]
    };
    let (outer, inner) = (arrow(look.edge_r), arrow(look.arrow_r));
    let is_dot = cell == DIRECTIONS;
    let inside = |tris: &[[(i32, i32); 3]; 2], p: (i32, i32)| {
        let edge =
            |a: (i32, i32), b: (i32, i32)| (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
        tris.iter().any(|t| {
            let e = [edge(t[0], t[1]), edge(t[1], t[2]), edge(t[2], t[0])];
            e.iter().all(|&v| v >= 0) || e.iter().all(|&v| v <= 0)
        })
    };
    let m = (mid * UNIT) as i32;
    let in_circle = |p: (i32, i32), r: f64| {
        let r = (r * UNIT) as i32;
        (p.0 - m) * (p.0 - m) + (p.1 - m) * (p.1 - m) <= r * r
    };
    let mut row = vec![0u8; size * 4];
    for y in 0..size as i32 {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let (mut out, mut fill) = (0u32, 0u32);
            for s in 0..16 {
                let p = (x as i32 * 64 + 8 + 16 * (s % 4), y * 64 + 8 + 16 * (s / 4));
                let (o, f) = if is_dot {
                    (in_circle(p, look.ring_r), in_circle(p, look.dot_r))
                } else {
                    (inside(&outer, p), inside(&inner, p))
                };
                out += u32::from(o || f);
                fill += u32::from(f);
            }
            *px = if look.paint == Paint::Palette {
                cursor_pixel(out, fill)
            } else if look.paint == Paint::One {
                // Opaque, so premultiplied as it is.
                if out >= 8 {
                    [one[2], one[1], one[0], 255]
                } else {
                    [0; 4]
                }
            } else if out == 0 {
                [0; 4]
            } else {
                // The fill over the edge, by how much of the covered
                // part is fill; alpha by how much is covered.
                let mix = |c: usize| {
                    ((u32::from(FILL[c]) * fill + u32::from(EDGE[c]) * (out - fill)) / out) as u8
                };
                [mix(2), mix(1), mix(0), ((out * 255 + 8) / 16) as u8]
            };
        }
        let at = (y0 + y as usize) * pitch + x0 * 4;
        if let Some(dst) = dst.get_mut(at..at + row.len()) {
            dst.copy_from_slice(&row);
        }
    }
}

/// A cursor pixel from `out` of 16 samples covered, `fill` of them by the
/// fill: alpha in quarters, the fill's share in quarters where it is
/// opaque and in halves at the rim. 14 colours at most, so the cursor's 15
/// hold every one exactly. Premultiplied, B G R A.
fn cursor_pixel(out: u32, fill: u32) -> [u8; 4] {
    let a = (out * 4 + 8) / 16;
    if a == 0 {
        return [0; 4];
    }
    let steps = if a == 4 { 4 } else { 2 };
    let k = (fill * steps + out / 2) / out;
    let alpha = a * 255 / 4;
    let c = |i: usize| {
        let mix = (u32::from(FILL[i]) * k + u32::from(EDGE[i]) * (steps - k)) / steps;
        ((mix * alpha + 127) / 255) as u8
    };
    [c(2), c(1), c(0), alpha as u8]
}

/// The arrow's cell for a heading (x right, y down): the nearest of the
/// directions, clockwise from up.
fn direction(hx: i64, hy: i64) -> usize {
    let t = (hx as f64).atan2(-(hy as f64));
    let k = (t / std::f64::consts::TAU * DIRECTIONS as f64).round() as i64;
    k.rem_euclid(DIRECTIONS as i64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_arrow_points_the_nearest_way_of_64() {
        assert_eq!(direction(0, -1024), 0);
        assert_eq!(direction(1024, 0), 16);
        assert_eq!(direction(0, 1024), 32);
        assert_eq!(direction(-1024, 0), 48);
        // Just short of up, the other way round: still up.
        assert_eq!(direction(-10, -1024), 0);
        assert_eq!(direction(724, -724), 8);
    }

    #[test]
    fn the_cursor_marker_fits_its_square_and_its_palette() {
        let size = SMALL.cell as usize;
        // The tip and the back corners inside the square, every way round.
        for look in [&SHEET, &SMALL, &MEDIUM, &LARGE] {
            let reach = look.edge_r.max(look.back.hypot(look.wide) * look.edge_r);
            assert!(reach <= f64::from(look.cell) / 2.0, "{reach}");
            assert!(look.ring_r <= f64::from(look.cell) / 2.0);
        }
        let mut colours = std::collections::BTreeSet::new();
        for cell in 0..=DIRECTIONS {
            let mut px = vec![0u8; size * size * 4];
            draw(&mut px, size * 4, (0, 0), &SMALL, cell, false);
            let mut opaque = 0;
            for p in px.as_chunks::<4>().0 {
                // Premultiplied: no colour above its alpha.
                assert!(p[..3].iter().all(|&c| c <= p[3]), "{p:?}");
                if p[3] != 0 {
                    colours.insert(*p);
                }
                opaque += usize::from(p[3] == 255);
            }
            assert!(opaque > 150, "cell {cell}: {opaque} opaque pixels");
        }
        // Every direction together: within the 15 the cursor has besides
        // transparent, so it shows them exactly.
        assert!(colours.len() <= 15, "{} colours", colours.len());
    }

    /// Every direction's opaque pixels, in the look's square.
    fn opaque(look: &Look, night: bool) -> Vec<usize> {
        let size = look.cell as usize;
        (0..=DIRECTIONS)
            .map(|cell| {
                let mut px = vec![0u8; size * size * 4];
                draw(&mut px, size * 4, (0, 0), look, cell, night);
                let one = if night { ONE_NIGHT } else { ONE_DAY };
                let colour = [one[2], one[1], one[0], 255];
                let chunks = px.as_chunks::<4>().0;
                // Two entries only: transparent and the colour.
                assert!(chunks.iter().all(|p| *p == [0; 4] || *p == colour));
                chunks.iter().filter(|p| p[3] == 255).count()
            })
            .collect()
    }

    #[test]
    fn the_large_cursors_are_one_colour_by_day_or_night() {
        let medium = opaque(&MEDIUM, false);
        let large = opaque(&LARGE, true);
        assert_eq!(opaque(&MEDIUM, true), medium);
        for cell in 0..DIRECTIONS {
            // The medium arrow as big as the sheet's, the large clearly
            // bigger: by area, a third longer is near 1.8 times.
            assert!(medium[cell] > 250, "cell {cell}: {}", medium[cell]);
            assert!(large[cell] * 10 > medium[cell] * 16, "cell {cell}");
        }
    }

    #[test]
    fn a_cursor_pixel_is_transparent_white_or_blue() {
        assert_eq!(cursor_pixel(0, 0), [0; 4]);
        assert_eq!(cursor_pixel(1, 0), [0; 4]);
        assert_eq!(cursor_pixel(16, 0), [255, 255, 255, 255]);
        assert_eq!(cursor_pixel(16, 16), [232, 115, 26, 255]);
        // Half covered, edge only: white at half alpha, premultiplied.
        assert_eq!(cursor_pixel(8, 0), [127, 127, 127, 127]);
    }
}
