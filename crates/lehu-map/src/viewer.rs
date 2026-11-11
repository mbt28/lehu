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
use crate::sprite::Sprites;
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
        // Heading-up frames: the panel and some road ahead and to the
        // sides, which the window moves over until the map turns again.
        // Without the memory for them the map stays north up.
        let up_size = (panel.0 + 2 * UP_SIDE, panel.1 + UP_ROOM);
        let mut ups = Vec::with_capacity(CANVASES);
        for _ in 0..CANVASES {
            match display.canvas(up_size.0, up_size.1, true) {
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
        // The marker on a plane of its own, when there is one: the map
        // then moves under it without a redraw.
        let sprites = Sprites::new(&display);
        if sprites.is_some() {
            log::info!("map: the marker is on the sprite plane");
        }
        display.hold_video(true);
        let thread = {
            let shared = Arc::clone(&shared);
            let display = Arc::clone(&display);
            let tiles = Tiles::new(set, hw);
            let frames = Frames {
                canvases,
                ups,
                sprites,
            };
            thread::Builder::new()
                .name("map".into())
                .spawn(move || run(&shared, &display, tiles, frames, panel, (cols, rows)))
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

    /// The view, where we are and the way up, all at once: one frame for
    /// the three, where setting them one by one could wake the thread for
    /// each.
    pub fn set(&self, view: View, position: Option<Marker>, heading: Option<(i32, i32)>) {
        let mut inner = self.shared.lock();
        if (inner.want, inner.position, inner.heading) != (view, position, heading) {
            (inner.want, inner.position, inner.heading) = (view, position, heading);
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
            let joined = t.join();
            // The thread commits no more: the marker off its plane before
            // its canvas goes.
            self.display.hide_sprite();
            match joined {
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

/// The thread's pictures: the north-up canvases, the heading-up frames,
/// and the marker's sprites.
struct Frames {
    canvases: Vec<Canvas>,
    ups: Vec<Canvas>,
    sprites: Option<Sprites>,
}

fn run(
    shared: &Shared,
    display: &Display,
    mut tiles: Tiles,
    frames: Frames,
    panel: (u32, u32),
    (cols, rows): (u32, u32),
) -> Vec<Canvas> {
    let Frames {
        mut canvases,
        mut ups,
        mut sprites,
    } = frames;
    // The heading-up frames shown last, as `recent` is for the canvases,
    // and what each holds; when the map last turned; and what it costs,
    // said every so often.
    let mut recent_up: [Option<usize>; 2] = [None, None];
    let mut up_held: Vec<Option<UpHeld>> = vec![None; ups.len()];
    let mut last_turn: Option<Instant> = None;
    let mut up_stats = UpStats::new();
    // A sprite canvas the driver would not show, kept to be freed.
    let mut retired: Vec<Canvas> = Vec::new();
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
        // The sprite plane can be refused at its first commit: from then
        // on the marker is drawn into the pictures.
        if sprites.is_some() && !display.has_sprite() {
            retired.extend(sprites.take().map(Sprites::into_canvas));
            held.fill(None);
            up_held.fill(None);
            shown = None;
            continue;
        }
        // The marker as the pictures hold it: none while it has a plane.
        let baked = if sprites.is_some() { None } else { position };
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
                    baked,
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
            ahead = None;
            let h = unit_up(up);
            let size = (ups[0].width(), ups[0].height());
            // A frame turned near enough this way that holds the window,
            // the one shown first: moving along is a commit, no drawing.
            // Without the sprite the marker is in the frame: each is new.
            let placed = if sprites.is_some() {
                recent_up
                    .iter()
                    .flatten()
                    .copied()
                    .chain(0..ups.len())
                    .find_map(|i| up_held[i]?.window(&want, h, size, panel).map(|w| (i, w)))
            } else {
                None
            };
            let (i, (x, y)) = match placed {
                Some(p) => p,
                None => {
                    let Some(i) = (0..ups.len()).find(|i| !recent_up.contains(&Some(*i))) else {
                        break;
                    };
                    // The window low in the frame, the road ahead above.
                    // Turning, the next turn is soon: the panel and a
                    // little more. Else all the room ahead; without the
                    // sprite, the panel alone.
                    let window = (UP_SIDE, size.1 - panel.1 - UP_BACK);
                    let top = if sprites.is_none() {
                        window.1
                    } else if last_turn.is_some_and(|t| t.elapsed() < UP_TURNING) {
                        window.1 - UP_TURN_ROOM
                    } else {
                        0
                    };
                    let started = Instant::now();
                    up_held[i] = Some(render_up(
                        &mut tiles,
                        &mut ups[i],
                        &want,
                        h,
                        baked,
                        route.as_deref(),
                        anchor_on(panel, window),
                        top,
                    ));
                    last_turn = Some(Instant::now());
                    up_stats.turned(started.elapsed());
                    (i, window)
                }
            };
            up_stats.frame();
            let sprite = match (&mut sprites, position, up_held[i]) {
                (Some(sp), Some(m), Some(u)) => {
                    let (at, heading) = parts(m);
                    let c = u.sixteenth(at);
                    let on_panel = (
                        ((c.0 + 8) >> 4) - i64::from(x),
                        ((c.1 + 8) >> 4) - i64::from(y),
                    );
                    Some(sp.at(heading.map(|m| u.turn(m)), on_panel))
                }
                _ => None,
            };
            // A pending flip: the same frame again a moment later.
            let mut tries = 0;
            loop {
                match display.show_canvas_with(&ups[i], x, y, sprite) {
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
                (h.marker == baked).then_some(())?;
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
                draw_marker(canvas.pixels(), pitch, chroma, size, &h.layout, baked);
                held[i] = Some(Held {
                    layout: h.layout,
                    marker: baked,
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
                    baked,
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
        // The marker on its plane over the window.
        let sprite = match (&mut sprites, position, held[index]) {
            (Some(sp), Some(m), Some(h)) => {
                let (at, heading) = parts(m);
                let c = h.layout.pixel(at);
                Some(sp.at(heading, (c.0 - i64::from(x), c.1 - i64::from(y))))
            }
            _ => None,
        };
        match display.show_canvas_with(&canvases[index], x, y, sprite) {
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
    canvases.extend(retired);
    canvases.extend(sprites.map(Sprites::into_canvas));
    canvases
}

/// A marker's point, and its heading if it has one.
fn parts(marker: Marker) -> ((i64, i64), Option<(i64, i64)>) {
    match marker {
        Marker::Dot(x, y) => ((x, y), None),
        Marker::Arrow(x, y, h) => ((x, y), Some((i64::from(h.0), i64::from(h.1)))),
    }
}

/// The heading-up frames' cost: frames shown, those turned anew and the
/// time that took, said every `UP_STATS_EVERY`.
struct UpStats {
    frames: u32,
    turned: u32,
    took: Duration,
    most: Duration,
    since: Instant,
}

impl UpStats {
    fn new() -> Self {
        Self {
            frames: 0,
            turned: 0,
            took: Duration::ZERO,
            most: Duration::ZERO,
            since: Instant::now(),
        }
    }

    fn turned(&mut self, took: Duration) {
        self.turned += 1;
        self.took += took;
        self.most = self.most.max(took);
    }

    fn frame(&mut self) {
        self.frames += 1;
        if self.since.elapsed() >= UP_STATS_EVERY {
            log::info!(
                "map: {} frames heading up in {} s, {} turned, {} ms each, {} ms at most",
                self.frames,
                self.since.elapsed().as_secs(),
                self.turned,
                self.took.as_millis() / u128::from(self.turned.max(1)),
                self.most.as_millis()
            );
            *self = Self::new();
        }
    }
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

/// How often the cost of the heading-up frames is logged.
const UP_STATS_EVERY: Duration = Duration::from_secs(30);

/// Where the view's point sits on the panel heading up, in percent: the
/// middle across, low down, for more road ahead than behind.
const UP_ANCHOR: (u32, u32) = (50, 69);

/// A heading-up frame is the panel and more: `UP_SIDE` pixels each side,
/// `UP_ROOM` rows more of which `UP_BACK` are behind the window as drawn,
/// the rest the road ahead it moves into before the map is turned anew.
const UP_SIDE: u32 = 32;
const UP_ROOM: u32 = 160;
const UP_BACK: u32 = 16;
/// While it turns (the last turn this recent), only `UP_TURN_ROOM` rows
/// ahead of the window are drawn: the next turn is soon.
const UP_TURNING: Duration = Duration::from_secs(1);
const UP_TURN_ROOM: u32 = 32;
/// How far the way up may be from a frame's before it is turned anew:
/// the sine of 0.8 degrees, times 2^32 (two 65536-long ways crossed).
const UP_TOLERANCE: i64 = 60_000_000;

/// The anchor on a frame whose window is at `window`.
fn anchor_on(panel: (u32, u32), window: (u32, u32)) -> (i64, i64) {
    let a = panel_anchor(panel);
    (a.0 + i64::from(window.0), a.1 + i64::from(window.1))
}

fn panel_anchor(panel: (u32, u32)) -> (i64, i64) {
    (
        i64::from(panel.0 * UP_ANCHOR.0 / 100),
        i64::from(panel.1 * UP_ANCHOR.1 / 100),
    )
}

/// The way up, 65536 long.
fn unit_up(up: (i32, i32)) -> (i64, i64) {
    let len = f64::from(up.0).hypot(f64::from(up.1)).max(1.0);
    (
        (f64::from(up.0) / len * 65536.0) as i64,
        (f64::from(up.1) / len * 65536.0) as i64,
    )
}

/// A world coordinate (pixels at `WORLD_Z`) in 1/65536 pixels of `zoom`.
fn fine(zoom: u8, v: i64) -> i64 {
    let s = i32::from(WORLD_Z - zoom.min(WORLD_Z)) - 16;
    if s >= 0 { v >> s } else { v << -s }
}

/// What a heading-up frame holds: the map at `zoom` turned so that `h`
/// (65536 long) is up, the point `p` (1/65536 pixels) at `anchor` (its
/// pixels), drawn from row `top` down.
#[derive(Clone, Copy, Debug)]
struct UpHeld {
    zoom: u8,
    p: (i64, i64),
    h: (i64, i64),
    anchor: (i64, i64),
    top: u32,
}

impl UpHeld {
    /// One pixel of the frame right, and down, on the map (1/65536 px).
    fn across(&self) -> (i64, i64) {
        (-self.h.1, self.h.0)
    }

    fn down(&self) -> (i64, i64) {
        (-self.h.0, -self.h.1)
    }

    /// A world point on the frame, in 1/16 pixels: from the anchor by its
    /// distance along each way. In i64: 2^16 by 2^16 a pixel, and a route
    /// would have to run a million pixels away to overflow.
    fn sixteenth(&self, (x, y): (i64, i64)) -> (i64, i64) {
        let (dx, dy) = (fine(self.zoom, x) - self.p.0, fine(self.zoom, y) - self.p.1);
        let (a, d) = (self.across(), self.down());
        (
            self.anchor.0 * 16 + ((dx * a.0 + dy * a.1) >> 28),
            self.anchor.1 * 16 + ((dx * d.0 + dy * d.1) >> 28),
        )
    }

    /// A heading on the map (x right, y down) as it lies on the frame.
    fn turn(&self, (mx, my): (i64, i64)) -> (i64, i64) {
        let (a, d) = (self.across(), self.down());
        ((mx * a.0 + my * a.1) >> 16, (mx * d.0 + my * d.1) >> 16)
    }

    /// Where the panel's window for `view`, `h` up, sits on this frame
    /// (`size`): when the frame is turned near enough that way and the
    /// window lies inside what it has drawn.
    fn window(
        &self,
        view: &View,
        h: (i64, i64),
        size: (u32, u32),
        panel: (u32, u32),
    ) -> Option<(u32, u32)> {
        if view.zoom != self.zoom {
            return None;
        }
        let cross = self.h.0 * h.1 - self.h.1 * h.0;
        let dot = self.h.0 * h.0 + self.h.1 * h.1;
        if dot <= 0 || cross.abs() > UP_TOLERANCE {
            return None;
        }
        let c = self.sixteenth((view.x, view.y));
        let a = panel_anchor(panel);
        let x = ((c.0 + 8) >> 4) - a.0;
        let y = ((c.1 + 8) >> 4) - a.1;
        let fits = x >= 0
            && y >= i64::from(self.top)
            && x + i64::from(panel.0) <= i64::from(size.0)
            && y + i64::from(panel.1) <= i64::from(size.1);
        fits.then_some((x as u32, y as u32))
    }
}

/// A heading-up frame of `view` on `out`, rows `top` (made even) down:
/// the tiles turned so that `h` points up, the view's point at `anchor`;
/// then the route and the marker (if it is to be drawn in), turned the
/// same way. What the frame then holds.
#[allow(clippy::too_many_arguments)]
fn render_up(
    tiles: &mut Tiles,
    out: &mut Canvas,
    view: &View,
    h: (i64, i64),
    marker: Option<Marker>,
    route: Option<&[(i64, i64)]>,
    anchor: (i64, i64),
    top: u32,
) -> UpHeld {
    let size = (out.width(), out.height());
    let (pitch, chroma) = (out.pitch(), out.chroma_offset());
    let top = top & !1;
    let held = UpHeld {
        zoom: view.zoom,
        p: (fine(view.zoom, view.x), fine(view.zoom, view.y)),
        h,
        anchor,
        top,
    };
    let (p, across, down) = (held.p, held.across(), held.down());
    // The drawn part's corners on the map, for the tiles under them.
    let corner = |u: i64, v: i64| {
        (
            p.0 + (u - anchor.0) * across.0 + (v - anchor.1) * down.0,
            p.1 + (u - anchor.0) * across.1 + (v - anchor.1) * down.1,
        )
    };
    let (w, ht, t) = (i64::from(size.0), i64::from(size.1), i64::from(top));
    let corners = [corner(0, t), corner(w, t), corner(0, ht), corner(w, ht)];
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
        // From row `top`: its luma, and its chroma's place from there.
        let skip = top as usize * pitch;
        nv12::rotate(
            &grid,
            &mut out.pixels()[skip..],
            pitch,
            chroma - (top as usize / 2) * pitch,
            (size.0, size.1 - top),
            origin,
            (anchor.0, anchor.1 - t),
            across,
            down,
        );
    }
    if let Some(line) = route {
        let points: Vec<(i64, i64)> = line.iter().map(|&q| held.sixteenth(q)).collect();
        nv12::route(out.pixels(), pitch, chroma, size, (0, t, w, ht), &points);
    }
    match marker {
        Some(Marker::Dot(x, y)) => {
            let at = held.sixteenth((x, y));
            nv12::dot(out.pixels(), pitch, chroma, size, (at.0 >> 4, at.1 >> 4));
        }
        Some(Marker::Arrow(x, y, m)) => {
            let at = held.sixteenth((x, y));
            let turned = held.turn((i64::from(m.0), i64::from(m.1)));
            nv12::arrow(
                out.pixels(),
                pitch,
                chroma,
                size,
                (at.0 >> 4, at.1 >> 4),
                (turned.0 as i32, turned.1 as i32),
            );
        }
        None => {}
    }
    held
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

#[cfg(test)]
mod tests {
    use super::*;

    const PANEL: (u32, u32) = (800, 480);
    const SIZE: (u32, u32) = (PANEL.0 + 2 * UP_SIDE, PANEL.1 + UP_ROOM);
    const WINDOW: (u32, u32) = (UP_SIDE, SIZE.1 - PANEL.1 - UP_BACK);

    fn frame(view: &View, h: (i64, i64), top: u32) -> UpHeld {
        UpHeld {
            zoom: view.zoom,
            p: (fine(view.zoom, view.x), fine(view.zoom, view.y)),
            h,
            anchor: anchor_on(PANEL, WINDOW),
            top,
        }
    }

    /// `view` moved `dx`, `dy` pixels of its zoom.
    fn moved(view: &View, dx: i64, dy: i64) -> View {
        let s = WORLD_Z - view.zoom;
        View {
            zoom: view.zoom,
            x: view.x + (dx << s),
            y: view.y + (dy << s),
        }
    }

    fn turned(degrees: f64) -> (i64, i64) {
        let r = degrees.to_radians();
        ((r.sin() * 65536.0) as i64, (-r.cos() * 65536.0) as i64)
    }

    /// Luxembourg City.
    const AT: View = View {
        zoom: 17,
        x: 555_154_349,
        y: 365_948_927,
    };

    #[test]
    fn a_frame_shows_its_own_view_where_it_was_drawn() {
        let f = frame(&AT, turned(0.0), 0);
        assert_eq!(f.window(&AT, turned(0.0), SIZE, PANEL), Some(WINDOW));
        let c = f.sixteenth((AT.x, AT.y));
        assert_eq!((c.0 >> 4, c.1 >> 4), anchor_on(PANEL, WINDOW));
    }

    #[test]
    fn driving_on_moves_the_window_up_the_frame_until_it_runs_out() {
        // Heading east, east is up the frame.
        let east = turned(90.0);
        let f = frame(&AT, east, 0);
        let on = moved(&AT, 50, 0);
        assert_eq!(
            f.window(&on, east, SIZE, PANEL),
            Some((WINDOW.0, WINDOW.1 - 50))
        );
        // A pixel to the side moves it across by one.
        let aside = moved(&AT, 50, 1);
        assert_eq!(
            f.window(&aside, east, SIZE, PANEL),
            Some((WINDOW.0 + 1, WINDOW.1 - 50))
        );
        // Past the room ahead: a new frame.
        let far = moved(&AT, i64::from(WINDOW.1) + 2, 0);
        assert_eq!(f.window(&far, east, SIZE, PANEL), None);
        // Nor above the rows drawn.
        let partly = frame(&AT, east, WINDOW.1 - UP_TURN_ROOM);
        let past = moved(&AT, i64::from(UP_TURN_ROOM) + 2, 0);
        assert_eq!(partly.window(&past, east, SIZE, PANEL), None);
    }

    #[test]
    fn turned_further_than_the_tolerance_is_a_new_frame() {
        let f = frame(&AT, turned(30.0), 0);
        assert!(f.window(&AT, turned(30.5), SIZE, PANEL).is_some());
        assert!(f.window(&AT, turned(29.5), SIZE, PANEL).is_some());
        assert_eq!(f.window(&AT, turned(31.0), SIZE, PANEL), None);
        assert_eq!(f.window(&AT, turned(210.0), SIZE, PANEL), None);
        // Another zoom never shows on it.
        let out = View { zoom: 16, ..AT };
        assert_eq!(f.window(&out, turned(30.0), SIZE, PANEL), None);
    }

    #[test]
    fn a_heading_turns_with_the_frame() {
        let f = frame(&AT, turned(90.0), 0);
        // East is up; south is right.
        assert_eq!(f.turn((65536, 0)), (0, -65536));
        assert_eq!(f.turn((0, 65536)), (65536, 0));
    }
}
