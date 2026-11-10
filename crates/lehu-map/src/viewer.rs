// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The map on the video plane, from a thread of its own.
//!
//! The loop hands over the view it wants; the thread shows the newest one
//! and skips those it fell behind on. A view whose window still lies on
//! the canvas shown costs one commit; otherwise the next canvas is composed
//! around it and flipped to. Three canvases take turns, so the one written
//! is never the one on scanout, nor the one the sun4i CRTC may still be
//! reading after an early flip event.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lehu_drm::{Canvas, Display, Show};

use crate::tiles::Tiles;
use crate::tileset::TileSet;
use crate::view::{Layout, TILE, View};

const CANVASES: usize = 3;

/// A flip is still pending: look again this much later.
const BUSY_WAIT: Duration = Duration::from_millis(4);

struct Inner {
    want: View,
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
    /// once. The decoder's frames are held off the plane until `stop`.
    pub fn start(display: Arc<Display>, set: TileSet, view: View) -> Result<Self, String> {
        let panel = (display.width(), display.height());
        // Enough whole tiles that the panel fits at any offset, and a tile
        // to spare each way: 1280 x 768 for 800 x 480.
        let cols = panel.0.div_ceil(TILE as u32) + 1;
        let rows = panel.1.div_ceil(TILE as u32) + 1;
        let (w, h) = (cols * TILE as u32, rows * TILE as u32);
        let mut canvases = Vec::with_capacity(CANVASES);
        for _ in 0..CANVASES {
            match display.canvas(w, h) {
                Ok(c) => canvases.push(c),
                Err(e) => {
                    for c in canvases {
                        display.free_canvas(c);
                    }
                    return Err(format!("No video memory for the map: {e}"));
                }
            }
        }
        let bpp = canvases[0].bytes_per_pixel() as usize;
        log::info!(
            "map: {} zoom {}-{}, canvases {w}x{h} at {} bytes a pixel",
            set.name,
            set.min_zoom,
            set.max_zoom,
            bpp
        );
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                want: view,
                stop: false,
                shown: false,
            }),
            wake: Condvar::new(),
        });
        display.hold_video(true);
        let thread = {
            let shared = Arc::clone(&shared);
            let display = Arc::clone(&display);
            let tiles = Tiles::new(set, bpp);
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
    // The canvas shown and what it holds; the next one to compose into.
    let mut current: Option<(usize, Layout)> = None;
    let mut next = 0;
    let mut shown: Option<View> = None;
    loop {
        let want = {
            let mut inner = shared.lock();
            while !inner.stop && shown == Some(inner.want) {
                inner = shared
                    .wake
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            if inner.stop {
                break;
            }
            inner.want
        };
        let placed = current.and_then(|(i, l)| l.window(&want, panel).map(|w| (i, w)));
        let (index, (x, y)) = match placed {
            Some(p) => p,
            None => {
                let layout = Layout::around(&want, panel, cols, rows);
                let index = next;
                next = (next + 1) % canvases.len();
                let started = Instant::now();
                let decoded = tiles.decoded;
                let canvas = &mut canvases[index];
                let pitch = canvas.pitch();
                tiles.compose(&layout, canvas.pixels(), pitch);
                log::debug!(
                    "map: zoom {} at {},{} composed in {} ms, {} decoded",
                    layout.zoom,
                    layout.tx,
                    layout.ty,
                    started.elapsed().as_millis(),
                    tiles.decoded - decoded
                );
                current = Some((index, layout));
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
                shared.lock().shown = true;
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
