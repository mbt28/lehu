// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The marker on a plane of its own, above the map: the map then moves
//! under it by a commit alone, never redrawn for it.
//!
//! On the cursor plane when the display has one: a 32 pixel square the
//! hardware draws over everything, with 15 colours. Two of them, a new
//! direction drawn into the one the cursor does not hold, since the driver
//! reads a picture only when it is handed a new one. Else on the sprite
//! plane: one ARGB8888 canvas holds the dot and the arrow in 64
//! directions, each drawn the first time it is wanted; the plane shows one
//! cell of it.

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
    /// Premultiplied, in at most 14 colours: the cursor's.
    cursor: bool,
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
    cursor: false,
};

/// On the cursor: the corners on the circle the cell holds, so it turns
/// every way inside 32 pixels, three quarters of the sheet's length.
const CURSOR: Look = Look {
    cell: CURSOR_SIZE,
    dot_r: 9.0,
    ring_r: 13.0,
    arrow_r: 13.1,
    edge_r: 15.5,
    back: 0.70,
    wide: 0.70,
    notch: 0.28,
    cursor: true,
};

pub struct Sprites {
    kind: Kind,
}

enum Kind {
    Sheet {
        canvas: Canvas,
        drawn: [bool; DIRECTIONS + 1],
    },
    /// Two canvases and the cell each holds.
    Cursor(Vec<(Canvas, Option<usize>)>),
}

impl Sprites {
    /// The cursor when there is one, else the sprite plane; None with
    /// neither, or no memory for them.
    pub fn new(display: &Display) -> Option<Self> {
        if display.has_cursor() {
            let two: Result<Vec<_>, _> = (0..2).map(|_| display.cursor_canvas()).collect();
            match two {
                Ok(c) => {
                    return Some(Self {
                        kind: Kind::Cursor(c.into_iter().map(|c| (c, None)).collect()),
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
            }),
            Err(e) => {
                log::warn!("map: no memory for the marker's sprite ({e}); drawn into the map");
                None
            }
        }
    }

    /// Where the marker is, for the log.
    pub fn plane(&self) -> &'static str {
        match self.kind {
            Kind::Sheet { .. } => "the sprite plane",
            Kind::Cursor(_) => "the cursor plane",
        }
    }

    /// The display still takes the marker on its plane: one can be
    /// refused at its first commit.
    pub fn usable(&self, display: &Display) -> bool {
        match self.kind {
            Kind::Sheet { .. } => display.has_sprite(),
            Kind::Cursor(_) => display.has_cursor(),
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
                    draw(canvas.pixels(), pitch, (x0, y0), &SHEET, cell);
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
            Kind::Cursor(bufs) => {
                let i = match bufs.iter().position(|b| b.1 == Some(cell)) {
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
                        draw(canvas.pixels(), pitch, (0, 0), &CURSOR, cell);
                        *holds = Some(cell);
                        i
                    }
                };
                Sprite {
                    canvas: &bufs[i].0,
                    sx: 0,
                    sy: 0,
                    w: CURSOR.cell,
                    h: CURSOR.cell,
                    x: place(x, &CURSOR),
                    y: place(y, &CURSOR),
                }
            }
        }
    }

    pub fn into_canvases(self) -> Vec<Canvas> {
        match self.kind {
            Kind::Sheet { canvas, .. } => vec![canvas],
            Kind::Cursor(bufs) => bufs.into_iter().map(|(c, _)| c).collect(),
        }
    }
}

/// Draw `cell` (a direction, or `DIRECTIONS` for the dot) as `look` has
/// it at (`x0`, `y0`) of `dst`, rows `pitch` bytes apart, 16 samples a
/// pixel for smooth edges. The corners in floating point, the samples in
/// integers (1/64 px): no FPU here.
fn draw(dst: &mut [u8], pitch: usize, (x0, y0): (usize, usize), look: &Look, cell: usize) {
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
            *px = if look.cursor {
                cursor_pixel(out, fill)
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
        let size = CURSOR.cell as usize;
        // The tip and the back corners inside the square, every way round.
        for look in [&SHEET, &CURSOR] {
            let reach = look.edge_r.max(look.back.hypot(look.wide) * look.edge_r);
            assert!(reach <= f64::from(look.cell) / 2.0, "{reach}");
            assert!(look.ring_r <= f64::from(look.cell) / 2.0);
        }
        let mut colours = std::collections::BTreeSet::new();
        for cell in 0..=DIRECTIONS {
            let mut px = vec![0u8; size * size * 4];
            draw(&mut px, size * 4, (0, 0), &CURSOR, cell);
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
