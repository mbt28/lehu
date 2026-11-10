// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map on the video plane, from a thread of its own.
//!
//! The loop hands over the view it wants; the thread shows the newest one
//! and skips those it fell behind on. A view whose window still lies on
//! a canvas (the one shown, or one shown before) costs one commit;
//! otherwise a canvas is composed around it and flipped to. There are
//! three, and the one written is never the one on scanout, nor the one the
//! sun4i CRTC may still be reading after an early flip event.

use std::sync::{Arc, Condvar, Mutex, PoisonError};

/// The route to draw: world pixels at `WORLD_Z`.
pub type Route = Arc<[(i64, i64)]>;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lehu_drm::{Canvas, Display, Show};

use crate::hw::HwJpeg;
use crate::nv12;
use crate::tiles::Tiles;
use crate::tileset::TileSet;
use crate::view::{Layout, TILE, View, WORLD_Z};

const CANVASES: usize = 3;

/// The largest tile file the video engine is handed (its input buffer);
/// a larger one decodes in software. Tiles are 5 to 30 KB.
const HW_INPUT: u32 = 256 * 1024;

/// A flip is still pending: look again this much later.
const BUSY_WAIT: Duration = Duration::from_millis(4);

/// Where we are, as the map shows it: world pixels at `WORLD_Z`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// A position that does not move: a dot.
    Dot(i64, i64),
    /// On the move: an arrow along the heading (x right, y down).
    Arrow(i64, i64, (i32, i32)),
}

struct Inner {
    want: View,
    /// Where we are.
    position: Option<Marker>,
    /// The route, drawn under the marker.
    route: Option<Route>,
    /// Heading-up: the way that is up on the panel (x right, y down),
    /// the view's point at the anchor; None for north-up.
    heading: Option<(i32, i32)>,
    stop: bool,
    /// A picture is on the plane.
    shown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    wake: Condvar,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The map shown on the video plane while it lives.
pub struct Viewer {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Vec<Canvas>>>,
    display: Arc<Display>,
}

impl Viewer {
    /// The canvases are made here, so a lack of video memory is said at
    /// once, and the video engine's JPEG decoder opened when there is one.
    /// The phone decoder's frames are held off the plane until `stop`.
    pub fn start(display: Arc<Display>, set: TileSet, view: View) -> Result<Self, String> {
        let panel = (display.width(), display.height());
        // Enough whole tiles that the panel fits at any offset, and a tile
        // to spare each way: 1280 x 768 for 800 x 480.
        let cols = panel.0.div_ceil(TILE as u32) + 1;
        let rows = panel.1.div_ceil(TILE as u32) + 1;
        let (w, h) = (cols * TILE as u32, rows * TILE as u32);
        let mut canvases = Vec::with_capacity(CANVASES);
        for _ in 0..CANVASES {
            match display.canvas(w, h, true) {
                Ok(c) => canvases.push(c),
                Err(e) => {
                    for c in canvases {
                        display.free_canvas(c);
                    }
                    return Err(format!("No video memory for the map: {e}"));
                }
            }
        }
        let hw = if set.is_jpeg() {
            match HwJpeg::open(TILE as u32, HW_INPUT) {
                Ok((hw, path)) => {
                    log::info!("map: tiles decode on the video engine ({path})");
                    Some(hw)
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    log::info!("map: no hardware JPEG decoder; tiles decode in software");
                    None
                }
                Err(e) => {
                    log::warn!(
                        "map: the hardware JPEG decoder would not start ({e}); tiles decode in software"
                    );
                    None
                }
            }
        } else {
            None
        };
        log::info!(
            "map: {} zoom {}-{}, NV12 canvases {w}x{h}",
            set.name,
            set.min_zoom,
            set.max_zoom,
        );
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                want: view,
                position: None,
                route: None,
                heading: None,
                stop: false,
                shown: false,
            }),
            wake: Condvar::new(),
        });
        // Heading-up frames: panel-sized, drawn whole each time. Without
        // the memory for them the map stays north up.
        let mut ups = Vec::with_capacity(CANVASES);
        for _ in 0..CANVASES {
            match display.canvas(panel.0, panel.1, true) {
                Ok(c) => ups.push(c),
                Err(e) => {
                    log::warn!("map: no video memory to turn the map ({e}); north up only");
                    for c in ups.drain(..) {
                        display.free_canvas(c);
                    }
                    break;
                }
            }
        }
        display.hold_video(true);
        let thread = {
            let shared = Arc::clone(&shared);
            let display = Arc::clone(&display);
            let tiles = Tiles::new(set, hw);
            thread::Builder::new()
                .name("map".into())
                .spawn(move || run(&shared, &display, tiles, canvases, ups, panel, (cols, rows)))
                .map_err(|e| format!("map thread: {e}"))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
            display,
        })
    }

    /// Show `view` next; a view not yet shown is replaced.
    pub fn set_view(&self, view: View) {
        let mut inner = self.shared.lock();
        if inner.want != view {
            inner.want = view;
            self.shared.wake.notify_one();
        }
    }

    /// Turn the map so that `heading` points up, the view's point low on
    /// the panel (more ahead than behind); None: north up, the view's point
    /// in the middle.
    pub fn set_heading(&self, heading: Option<(i32, i32)>) {
        let mut inner = self.shared.lock();
        if inner.heading != heading {
            inner.heading = heading;
            self.shared.wake.notify_one();
        }
    }

    /// Draw this route (or none) under the marker.
    pub fn set_route(&self, route: Option<Route>) {
        let mut inner = self.shared.lock();
        let same = match (&inner.route, &route) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            inner.route = route;
            self.shared.wake.notify_one();
        }
    }

    /// Show where we are, or nothing.
    pub fn set_position(&self, position: Option<Marker>) {
        let mut inner = self.shared.lock();
        if inner.position != position {
            inner.position = position;
            self.shared.wake.notify_one();
        }
    }

    /// The first picture is on the plane.
    pub fn picture_ready(&self) -> bool {
        self.shared.lock().shown
    }

    /// End the thread and free the canvases. The screens should cover the
    /// panel first: the plane then loses its picture.
    pub fn stop(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        {
            let mut inner = self.shared.lock();
            inner.stop = true;
            self.shared.wake.notify_one();
        }
        if let Some(t) = self.thread.take() {
            match t.join() {
                Ok(canvases) => {
                    for c in canvases {
                        self.display.free_canvas(c);
                    }
                }
                Err(_) => log::error!("map: the thread panicked"),
            }
            self.display.hold_video(false);
        }
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.finish();
    }
}

/// What a canvas holds: its tiles, and the marker drawn on them.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Held {
    layout: Layout,
    marker: Option<Marker>,
}

#[allow(clippy::too_many_arguments)]
fn run(
    shared: &Shared,
    display: &Display,
    mut tiles: Tiles,
    mut canvases: Vec<Canvas>,
    mut ups: Vec<Canvas>,
    panel: (u32, u32),
    (cols, rows): (u32, u32),
) -> Vec<Canvas> {
    // The heading-up frames shown last, as `recent` is for the canvases.
    let mut recent_up: [Option<usize>; 2] = [None, None];
    let mut heading: Option<(i32, i32)> = None;
    // What each canvas holds, and the last two shown, newest first: those
    // are never written (the one on scanout, and the one the sun4i CRTC
    // may still be reading after an early flip event).
    let mut held: Vec<Option<Held>> = vec![None; canvases.len()];
    let mut recent: [Option<usize>; 2] = [None, None];
    let mut shown: Option<View> = None;
    // Where the view was going, and the canvas to have ready for where it
    // leaves the one shown.
    let mut heading_of_view = (0i64, 0i64);
    let mut ahead: Option<Layout> = None;
    // Where we are, as the canvases should show it, and the route.
    let mut position: Option<Marker> = None;
    let mut route: Option<Route> = None;
    loop {
        let want = {
            let mut inner = shared.lock();
            loop {
                let new_route = match (&inner.route, &route) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                };
                if new_route {
                    // Every canvas has the old one: all composed afresh.
                    route = inner.route.clone();
                    held.fill(None);
                    ahead = None;
                    shown = None;
                }
                if inner.heading != heading {
                    heading = inner.heading.filter(|_| !ups.is_empty());
                    shown = None;
                }
                if inner.position != position {
                    // The marker moved: shown again, on a canvas brought up
                    // to date.
                    position = inner.position;
                    shown = None;
                }
                if inner.stop || shown != Some(inner.want) || ahead.is_some() {
                    break;
                }
                inner = shared
                    .wake
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if inner.stop {
                break;
            }
            (shown != Some(inner.want)).then_some(inner.want)
        };
        let Some(want) = want else {
            // Nothing new to show: a step towards the canvas ahead.
            if let Some(layout) = ahead
                && !tiles.prepare(&layout)
            {
                // Every tile kept: the canvas composed from them.
                ahead = None;
                let started = Instant::now();
                if let Some(i) = compose_free(
                    &mut tiles,
                    &mut canvases,
                    &mut held,
                    &recent,
                    &layout,
                    position,
                    route.as_deref(),
                ) {
                    log::debug!(
                        "map: zoom {} at {},{} ready ahead in {} ms (canvas {i})",
                        layout.zoom,
                        layout.tx,
                        layout.ty,
                        started.elapsed().as_millis()
                    );
                }
            }
            continue;
        };
        if let Some(up) = heading {
            // Heading up: a whole frame, turned, into a free frame buffer.
            ahead = None;
            let Some(i) = (0..ups.len()).find(|i| !recent_up.contains(&Some(*i))) else {
                break;
            };
            let started = Instant::now();
            render_up(
                &mut tiles,
                &mut ups[i],
                &want,
                up,
                position,
                route.as_deref(),
            );
            log::debug!(
                "map: zoom {} heading up in {} ms",
                want.zoom,
                started.elapsed().as_millis()
            );
            // A pending flip: the same frame again a moment later.
            let mut tries = 0;
            loop {
                match display.show_canvas(&ups[i], 0, 0) {
                    Show::Shown => {
                        recent_up = [Some(i), recent_up[0]];
                        shared.lock().shown = true;
                    }
                    Show::Busy if tries < 50 => {
                        tries += 1;
                        thread::sleep(BUSY_WAIT);
                        continue;
                    }
                    Show::Busy | Show::Failed => {}
                }
                break;
            }
            shown = Some(want);
            continue;
        }
        if let Some(last) = shown {
            heading_of_view = if last.zoom == want.zoom {
                let s = WORLD_Z - want.zoom.min(WORLD_Z);
                ((want.x >> s) - (last.x >> s), (want.y >> s) - (last.y >> s))
            } else {
                (0, 0)
            };
        }
        let free = (0..canvases.len()).find(|i| !recent.contains(&Some(*i)));
        // A canvas that already holds the window and the marker, the one
        // shown first: scrolling back over an edge flips back, with no copy.
        let placed = recent
            .iter()
            .flatten()
            .copied()
            .chain(0..canvases.len())
            .find_map(|i| {
                let h = held[i]?;
                (h.marker == position).then_some(())?;
                h.layout.window(&want, panel).map(|w| (i, w))
            })
            // Else the free canvas, if it holds the window: only the
            // marker brought up to date, a patch where it was and is.
            .or_else(|| {
                let i = free?;
                let h = held[i]?;
                let w = h.layout.window(&want, panel)?;
                let canvas = &mut canvases[i];
                let (pitch, chroma) = (canvas.pitch(), canvas.chroma_offset());
                let size = (canvas.width(), canvas.height());
                if let Some(old) = h.marker {
                    let rect = marker_rect(&h.layout, old);
                    tiles.compose_rect(&h.layout, canvas.pixels(), pitch, chroma, rect);
                    if let Some(line) = route.as_deref() {
                        draw_route(canvas.pixels(), pitch, chroma, size, &h.layout, rect, line);
                    }
                }
                draw_marker(canvas.pixels(), pitch, chroma, size, &h.layout, position);
                held[i] = Some(Held {
                    layout: h.layout,
                    marker: position,
                });
                Some((i, w))
            });
        let (index, (x, y)) = match placed {
            Some(p) => p,
            None => {
                let layout = Layout::around(&want, panel, cols, rows);
                let started = Instant::now();
                let (decoded, software, enlarged) = (tiles.decoded, tiles.software, tiles.enlarged);
                let Some(index) = compose_free(
                    &mut tiles,
                    &mut canvases,
                    &mut held,
                    &recent,
                    &layout,
                    position,
                    route.as_deref(),
                ) else {
                    log::error!("map: no canvas free to compose into");
                    break;
                };
                log::debug!(
                    "map: zoom {} at {},{} composed in {} ms, {} decoded, {} in software, {} enlarged",
                    layout.zoom,
                    layout.tx,
                    layout.ty,
                    started.elapsed().as_millis(),
                    tiles.decoded - decoded,
                    tiles.software - software,
                    tiles.enlarged - enlarged
                );
                match layout.window(&want, panel) {
                    Some(w) => (index, w),
                    None => {
                        // Cannot be: the layout is made around the view.
                        log::error!("map: the view is off its own canvas");
                        shown = Some(want);
                        continue;
                    }
                }
            }
        };
        match display.show_canvas(&canvases[index], x, y) {
            Show::Shown => {
                shown = Some(want);
                if recent[0] != Some(index) {
                    recent = [Some(index), recent[0]];
                }
                shared.lock().shown = true;
                // Ready the canvas the view is heading for, unless one
                // already holds it.
                ahead = held[index]
                    .and_then(|h| h.layout.ahead(&want, heading_of_view, panel))
                    .filter(|next| !held.iter().flatten().any(|h| h.layout == *next));
            }
            Show::Busy => thread::sleep(BUSY_WAIT),
            Show::Failed => {
                // Said once by the display; not retried until the view moves.
                shown = Some(want);
            }
        }
    }
    canvases.extend(ups);
    canvases
}

/// The marker on a canvas: the dot, or the arrow along its heading.
fn draw_marker(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    layout: &Layout,
    marker: Option<Marker>,
) {
    match marker {
        Some(Marker::Dot(x, y)) => nv12::dot(dst, pitch, chroma, size, layout.pixel((x, y))),
        Some(Marker::Arrow(x, y, heading)) => {
            nv12::arrow(dst, pitch, chroma, size, layout.pixel((x, y)), heading);
        }
        None => {}
    }
}

/// Where the view's point sits on the panel heading up, in percent: the
/// middle across, low down, for more road ahead than behind.
const UP_ANCHOR: (u32, u32) = (50, 69);

/// A heading-up frame of `view` on `out`: the tiles turned so that `up`
/// points up, the view's point at the anchor; then the route and the
/// marker, turned the same way.
fn render_up(
    tiles: &mut Tiles,
    out: &mut Canvas,
    view: &View,
    up: (i32, i32),
    position: Option<Marker>,
    route: Option<&[(i64, i64)]>,
) {
    let size = (out.width(), out.height());
    let (pitch, chroma) = (out.pitch(), out.chroma_offset());
    let anchor = (
        i64::from(size.0 * UP_ANCHOR.0 / 100),
        i64::from(size.1 * UP_ANCHOR.1 / 100),
    );
    // A world point (WORLD_Z pixels) in 1/65536 pixels of this zoom.
    let s = i32::from(WORLD_Z - view.zoom.min(WORLD_Z)) - 16;
    let fine = |v: i64| if s >= 0 { v >> s } else { v << -s };
    let p = (fine(view.x), fine(view.y));
    // `up`, and the way right of it, each 65536 long.
    let len = f64::from(up.0).hypot(f64::from(up.1)).max(1.0);
    let h = (
        (f64::from(up.0) / len * 65536.0) as i64,
        (f64::from(up.1) / len * 65536.0) as i64,
    );
    let across = (-h.1, h.0);
    let down = (-h.0, -h.1);
    // The panel's corners on the map, for the tiles under them.
    let corner = |u: i64, v: i64| {
        (
            p.0 + (u - anchor.0) * across.0 + (v - anchor.1) * down.0,
            p.1 + (u - anchor.0) * across.1 + (v - anchor.1) * down.1,
        )
    };
    let (w, ht) = (i64::from(size.0), i64::from(size.1));
    let corners = [corner(0, 0), corner(w, 0), corner(0, ht), corner(w, ht)];
    let tile = TILE << 16;
    let xs = corners.iter().map(|c| c.0);
    let ys = corners.iter().map(|c| c.1);
    let tx0 = xs.clone().min().unwrap_or(0).div_euclid(tile);
    let tx1 = xs.max().unwrap_or(0).div_euclid(tile);
    let ty0 = ys.clone().min().unwrap_or(0).div_euclid(tile);
    let ty1 = ys.max().unwrap_or(0).div_euclid(tile);
    let (cols, rows) = ((tx1 - tx0 + 1) as usize, (ty1 - ty0 + 1) as usize);
    {
        let grid = tiles.grid(view.zoom, (tx0, ty0), cols, rows);
        let origin = (p.0 - tx0 * tile, p.1 - ty0 * tile);
        nv12::rotate(
            &grid,
            out.pixels(),
            pitch,
            chroma,
            size,
            origin,
            anchor,
            across,
            down,
        );
    }
    // A map point on the panel, in 1/16 pixels: from the anchor by its
    // distance along each way.
    let to_panel = |x: i64, y: i64| {
        let (dx, dy) = (i128::from(fine(x) - p.0), i128::from(fine(y) - p.1));
        let a = (dx * i128::from(across.0) + dy * i128::from(across.1)) >> 28;
        let d = (dx * i128::from(down.0) + dy * i128::from(down.1)) >> 28;
        (anchor.0 * 16 + a as i64, anchor.1 * 16 + d as i64)
    };
    if let Some(line) = route {
        let points: Vec<(i64, i64)> = line.iter().map(|&(x, y)| to_panel(x, y)).collect();
        nv12::route(out.pixels(), pitch, chroma, size, (0, 0, w, ht), &points);
    }
    match position {
        Some(Marker::Dot(x, y)) => {
            let at = to_panel(x, y);
            nv12::dot(out.pixels(), pitch, chroma, size, (at.0 >> 4, at.1 >> 4));
        }
        Some(Marker::Arrow(x, y, m)) => {
            let at = to_panel(x, y);
            // Its heading on the panel, along each way as for points.
            let (mx, my) = (i64::from(m.0), i64::from(m.1));
            let turned = (
                ((mx * across.0 + my * across.1) >> 16) as i32,
                ((mx * down.0 + my * down.1) >> 16) as i32,
            );
            nv12::arrow(
                out.pixels(),
                pitch,
                chroma,
                size,
                (at.0 >> 4, at.1 >> 4),
                turned,
            );
        }
        None => {}
    }
}

/// The route on a canvas, only inside `clip` (its pixels).
fn draw_route(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    layout: &Layout,
    clip: (i64, i64, i64, i64),
    line: &[(i64, i64)],
) {
    let points: Vec<(i64, i64)> = line.iter().map(|&p| layout.sixteenth(p)).collect();
    nv12::route(dst, pitch, chroma, size, clip, &points);
}

/// The part of a canvas a marker covers, in its pixels: left, top, right,
/// bottom.
fn marker_rect(layout: &Layout, marker: Marker) -> (i64, i64, i64, i64) {
    let (Marker::Dot(x, y) | Marker::Arrow(x, y, _)) = marker;
    let (cx, cy) = layout.pixel((x, y));
    let r = nv12::MARKER_REACH;
    (cx - r, cy - r, cx + r, cy + r)
}

/// Compose `layout` into a canvas that is neither on scanout nor just
/// replaced, with the marker at `position`, and say which; None when there
/// is none such.
fn compose_free(
    tiles: &mut Tiles,
    canvases: &mut [Canvas],
    held: &mut [Option<Held>],
    recent: &[Option<usize>; 2],
    layout: &Layout,
    position: Option<Marker>,
    route: Option<&[(i64, i64)]>,
) -> Option<usize> {
    let index = (0..canvases.len()).find(|i| !recent.contains(&Some(*i)))?;
    let canvas = &mut canvases[index];
    let (pitch, chroma) = (canvas.pitch(), canvas.chroma_offset());
    let size = (canvas.width(), canvas.height());
    held[index] = None;
    tiles.compose(layout, canvas.pixels(), pitch, chroma);
    if let Some(line) = route {
        let whole = (0, 0, i64::from(size.0), i64::from(size.1));
        draw_route(canvas.pixels(), pitch, chroma, size, layout, whole, line);
    }
    draw_marker(canvas.pixels(), pitch, chroma, size, layout, position);
    held[index] = Some(Held {
        layout: *layout,
        marker: position,
    });
    Some(index)
}
