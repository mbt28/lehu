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
                stop: false,
                shown: false,
            }),
            wake: Condvar::new(),
        });
        display.hold_video(true);
        let thread = {
            let shared = Arc::clone(&shared);
            let display = Arc::clone(&display);
            let tiles = Tiles::new(set, hw);
            thread::Builder::new()
                .name("map".into())
                .spawn(move || run(&shared, &display, tiles, canvases, panel, (cols, rows)))
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

fn run(
    shared: &Shared,
    display: &Display,
    mut tiles: Tiles,
    mut canvases: Vec<Canvas>,
    panel: (u32, u32),
    (cols, rows): (u32, u32),
) -> Vec<Canvas> {
    // What each canvas holds, and the last two shown, newest first: those
    // are never written (the one on scanout, and the one the sun4i CRTC
    // may still be reading after an early flip event).
    let mut held: Vec<Option<Layout>> = vec![None; canvases.len()];
    let mut recent: [Option<usize>; 2] = [None, None];
    let mut shown: Option<View> = None;
    // Where the view was going, and the canvas to have ready for where it
    // leaves the one shown.
    let mut heading = (0i64, 0i64);
    let mut ahead: Option<Layout> = None;
    // The position the canvases show.
    let mut position: Option<Marker> = None;
    loop {
        let want = {
            let mut inner = shared.lock();
            loop {
                if inner.position != position {
                    // Every canvas shows the old one: all composed afresh.
                    position = inner.position;
                    held.fill(None);
                    ahead = None;
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
                ) {
                    log::info!(
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
        if let Some(last) = shown {
            heading = if last.zoom == want.zoom {
                let s = WORLD_Z - want.zoom.min(WORLD_Z);
                ((want.x >> s) - (last.x >> s), (want.y >> s) - (last.y >> s))
            } else {
                (0, 0)
            };
        }
        // A canvas that already holds the window, the one shown first:
        // scrolling back over an edge flips back, with no copy.
        let placed = recent
            .iter()
            .flatten()
            .copied()
            .chain(0..canvases.len())
            .find_map(|i| held[i]?.window(&want, panel).map(|w| (i, w)));
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
                ) else {
                    log::error!("map: no canvas free to compose into");
                    break;
                };
                // At info while the decode is being measured on the board.
                log::info!(
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
                    .and_then(|l| l.ahead(&want, heading, panel))
                    .filter(|next| !held.contains(&Some(*next)));
            }
            Show::Busy => thread::sleep(BUSY_WAIT),
            Show::Failed => {
                // Said once by the display; not retried until the view moves.
                shown = Some(want);
            }
        }
    }
    canvases
}

/// Compose `layout` into a canvas that is neither on scanout nor just
/// replaced, with the dot at `position`, and say which; None when there is
/// none such.
fn compose_free(
    tiles: &mut Tiles,
    canvases: &mut [Canvas],
    held: &mut [Option<Layout>],
    recent: &[Option<usize>; 2],
    layout: &Layout,
    position: Option<Marker>,
) -> Option<usize> {
    let index = (0..canvases.len()).find(|i| !recent.contains(&Some(*i)))?;
    let canvas = &mut canvases[index];
    let (pitch, chroma) = (canvas.pitch(), canvas.chroma_offset());
    let size = (canvas.width(), canvas.height());
    held[index] = None;
    tiles.compose(layout, canvas.pixels(), pitch, chroma);
    match position {
        Some(Marker::Dot(x, y)) => {
            nv12::dot(canvas.pixels(), pitch, chroma, size, layout.pixel((x, y)));
        }
        Some(Marker::Arrow(x, y, heading)) => {
            let at = layout.pixel((x, y));
            nv12::arrow(canvas.pixels(), pitch, chroma, size, at, heading);
        }
        None => {}
    }
    held[index] = Some(*layout);
    Some(index)
}
