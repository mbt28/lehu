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
/// The speed cameras to draw: world pixels at `WORLD_Z`, in the order of
/// their x.
pub type CameraMarks = Arc<[(i64, i64)]>;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use lehu_drm::{Canvas, Display, Show};

use crate::drive::{Drive, NEAR_TURN_M, SpeedZoom, Steer};
use crate::hw::HwJpeg;
use crate::labels::Names;
use crate::nv12::{self, Surface};
use crate::sprite::Sprites;
use crate::tiles::Tiles;
use crate::tileset::TileSet;
use crate::view::{Layout, TILE, View, WORLD_Z};

const CANVASES: usize = 3;

/// The speed cameras are drawn from this zoom in: further out they would
/// crowd a city.
pub const CAMERA_ZOOM: u8 = 13;

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

/// How far along the route the drive is: the track point it heads for,
/// and where it is. The route behind it is drawn grey.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Progress {
    step: usize,
    at: (i64, i64),
}

struct Inner {
    want: View,
    /// A drive the map moves along by itself, and the zoom it follows it
    /// at; None: it does not follow, the view is the loop's.
    drive: Option<Drive>,
    follow: Option<u8>,
    progress: Option<Progress>,
    /// The night tiles.
    night: bool,
    /// Where we are.
    position: Option<Marker>,
    /// The route, drawn under the marker.
    route: Option<Route>,
    /// The speed cameras, drawn over the names.
    cameras: Option<CameraMarks>,
    /// Heading-up: the way that is up on the panel (x right, y down),
    /// the view's point at the anchor; None for north-up.
    heading: Option<(i32, i32)>,
    /// The way up `SOON_MS` from now, while the map turns by itself: a
    /// frame turned now is drawn for it, being shown about then.
    heading_soon: Option<(i32, i32)>,
    stop: bool,
    /// A picture is on the plane.
    shown: bool,
}

struct Shared {
    inner: Mutex<Inner>,
    /// The map thread's, and the drive thread's.
    wake: Condvar,
    drive_wake: Condvar,
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
    driver: Option<JoinHandle<()>>,
    display: Arc<Display>,
}

impl Viewer {
    /// The canvases are made here, so a lack of video memory is said at
    /// once, and the video engine's JPEG decoder opened when there is one.
    /// The phone decoder's frames are held off the plane until `stop`.
    /// `night`: the night tiles, shown while `set_night` says so.
    pub fn start(
        display: Arc<Display>,
        set: TileSet,
        night: Option<TileSet>,
        view: View,
    ) -> Result<Self, String> {
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
        // Names as data, drawn here; else they are in the tiles.
        let names = Names::open(set.dir());
        if names.is_some() {
            log::info!("map: the names are drawn over the tiles");
        }
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                want: view,
                drive: None,
                follow: None,
                progress: None,
                night: false,
                position: None,
                route: None,
                cameras: None,
                heading: None,
                heading_soon: None,
                stop: false,
                shown: false,
            }),
            wake: Condvar::new(),
            drive_wake: Condvar::new(),
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
            let tiles = Tiles::new(set, night, hw);
            let frames = Frames {
                canvases,
                ups,
                sprites,
            };
            thread::Builder::new()
                .name("map".into())
                .spawn(move || run(&shared, &display, tiles, names, frames, panel, (cols, rows)))
                .map_err(|e| format!("map thread: {e}"))?
        };
        let driver = {
            let shared = Arc::clone(&shared);
            let display = Arc::clone(&display);
            thread::Builder::new()
                .name("map-drive".into())
                .spawn(move || drive(&shared, &display))
                .map_err(|e| format!("map drive thread: {e}"))?
        };
        Ok(Self {
            shared,
            thread: Some(thread),
            driver: Some(driver),
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
    /// The speed cameras to draw (from [`CAMERA_ZOOM`] in), or none.
    pub fn set_cameras(&self, cameras: Option<CameraMarks>) {
        let mut inner = self.shared.lock();
        let same = match (&inner.cameras, &cameras) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            inner.cameras = cameras;
            self.shared.wake.notify_one();
        }
    }

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

    /// Move along `drive` by itself, or stop doing so: the marker, and the
    /// view too while it follows.
    pub fn set_drive(&self, drive: Option<Drive>) {
        let mut inner = self.shared.lock();
        if drive.is_none() {
            inner.progress = None;
        }
        inner.drive = drive;
        self.shared.drive_wake.notify_one();
        self.shared.wake.notify_one();
    }

    /// Follow the drive at `zoom` (a level closer near a turn), heading
    /// up; None: stay where `set_view` puts it, north up.
    pub fn follow(&self, zoom: Option<u8>) {
        let mut inner = self.shared.lock();
        inner.follow = zoom;
        if zoom.is_none() {
            inner.heading = None;
        }
        self.shared.wake.notify_one();
    }

    /// The view shown, or about to be: while it follows a drive, the
    /// drive's.
    pub fn view(&self) -> View {
        self.shared.lock().want
    }

    /// The night tiles, or the day's.
    pub fn set_night(&self, night: bool) {
        let mut inner = self.shared.lock();
        if inner.night != night {
            inner.night = night;
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
            self.shared.drive_wake.notify_one();
        }
        if let Some(t) = self.driver.take()
            && t.join().is_err()
        {
            log::error!("map: the drive thread panicked");
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

/// What a canvas holds: its tiles, the marker drawn on them, and where
/// the route's grey part ends.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Held {
    layout: Layout,
    marker: Option<Marker>,
    progress: Option<Progress>,
}

/// The route's grey part may end this many pixels behind the marker (the
/// arrow covers it) before the picture is drawn anew.
const ROUTE_LAG: i64 = 12;

/// A picture's grey part, drawn to `drawn`, still meets the marker at
/// `now`: within `ROUTE_LAG` pixels at `zoom`. Always, without a route.
fn fresh(drawn: Option<Progress>, now: Option<Progress>, route: bool, zoom: u8) -> bool {
    fresh_within(drawn, now, route, zoom, ROUTE_LAG)
}

/// While a frame is drawn anew, the one shown may lag this many pixels:
/// a little of the route behind the arrow still blue, for a moment.
const ROUTE_LAG_LOOSE: i64 = 4 * ROUTE_LAG;

/// As `fresh`, within `lag` pixels.
fn fresh_within(
    drawn: Option<Progress>,
    now: Option<Progress>,
    route: bool,
    zoom: u8,
    lag: i64,
) -> bool {
    if !route {
        return true;
    }
    match (drawn, now) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            let s = WORLD_Z - zoom.min(WORLD_Z);
            ((a.at.0 - b.at.0).abs() >> s) + ((a.at.1 - b.at.1).abs() >> s) <= lag
        }
        _ => false,
    }
}

/// The drive thread: while there is a drive, every second refresh of the
/// panel, where it is now, as the marker, the route's progress, and the
/// view and the way up while it follows; the map thread then shows that
/// before the next refresh, so the map moves in even steps. Above the map
/// thread's drawing (a little real-time priority, when it may have it), so
/// a turn being drawn does not make it late.
fn drive(shared: &Shared, display: &Display) {
    let param = libc::sched_param { sched_priority: 2 };
    // SAFETY: plain call on this thread's own handle; a refusal (no
    // RLIMIT_RTPRIO) leaves it as it was.
    unsafe {
        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
    }
    let mut steer = Steer::default();
    let mut speed_zoom = SpeedZoom::default();
    // When the last picture worked out was to be shown.
    let mut last: Option<Instant> = None;
    let mut next: Option<u32> = None;
    // The last refresh seen (its number, its time), and the time between
    // two: a picture is worked out for the refresh it will be shown at.
    let mut seen: Option<(u32, Duration)> = None;
    let mut period = Duration::from_micros(16_667);
    // Refreshes to the next look: one while the map moves (as fast as the
    // panel shows), a few while it stands still.
    let mut every = 1;
    loop {
        let drive = {
            let mut inner = shared.lock();
            loop {
                if inner.stop {
                    return;
                }
                if let Some(d) = &inner.drive {
                    break d.clone();
                }
                (last, next, seen) = (None, None, None);
                speed_zoom = SpeedZoom::default();
                inner = shared
                    .drive_wake
                    .wait(inner)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        };
        // When the picture worked out now will be on the panel: the next
        // refresh, by the refreshes' own times, so that each moves the map
        // the same way however late this thread woke.
        let now = match display.wait_vblank_time(next) {
            Ok((n, at)) => {
                next = Some(n.wrapping_add(every));
                if let Some((pn, pat)) = seen
                    && n.wrapping_sub(pn) > 0
                    && n.wrapping_sub(pn) < 8
                    && at > pat
                {
                    let p = (at - pat) / n.wrapping_sub(pn);
                    // A panel between 30 and 120 Hz.
                    if (Duration::from_millis(8)..Duration::from_millis(34)).contains(&p) {
                        period = p;
                    }
                }
                seen = Some((n, at));
                let woke = Instant::now();
                let ahead = (at + period).checked_sub(monotonic()).unwrap_or_default();
                woke + ahead
            }
            Err(_) => {
                // No refresh count: a sixtieth of a second, roughly.
                thread::sleep(Duration::from_millis(16 * u64::from(every)));
                next = None;
                Instant::now()
            }
        };
        let ms = now.saturating_duration_since(drive.start).as_millis() as u64;
        let fix = drive.track.at(ms);
        let since = last.map_or(0, |l| now.saturating_duration_since(l).as_millis() as u64);
        last = Some(now);
        let heading = steer.step(fix.heading, since);
        // Where the way up will be once a frame turned now is shown.
        let soon = {
            let mut ahead = steer;
            ahead.step(fix.heading, SOON_MS)
        };
        let marker = Some(Marker::Arrow(fix.x, fix.y, heading.unwrap_or(fix.heading)));
        let progress = Some(Progress {
            step: fix.step,
            at: (fix.x, fix.y),
        });
        let near = drive
            .track
            .ahead(ms)
            .is_some_and(|a| a.metres < NEAR_TURN_M);
        let mut inner = shared.lock();
        // Following, as it is now: the loop may just have stopped it.
        let (want, up, up_soon) = match inner.follow {
            Some(base) => {
                // The zoom chosen, a level closer in town and further out
                // fast; near a turn a level closer than chosen, as town is
                // already (so the zoom stays through a town's turns).
                let top = drive.top_zoom.max(base);
                let by_speed = base
                    .saturating_add_signed(speed_zoom.step(fix.kmh, ms))
                    .clamp(1, top);
                let zoom = if near {
                    by_speed.max(base.saturating_add(1)).min(top)
                } else {
                    by_speed
                };
                let mut v = View {
                    zoom,
                    x: fix.x,
                    y: fix.y,
                };
                v.clamp(&drive.bounds);
                (v, heading, soon)
            }
            None => (inner.want, None, None),
        };
        inner.heading_soon = up_soon;
        if (inner.want, inner.position, inner.heading, inner.progress)
            != (want, marker, up, progress)
        {
            (inner.want, inner.position, inner.heading, inner.progress) =
                (want, marker, up, progress);
            shared.wake.notify_one();
            every = 1;
        } else {
            every = STILL_EVERY;
        }
    }
}

/// The monotonic clock, as the kernel stamps refreshes with it.
fn monotonic() -> Duration {
    let mut t = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime fills the timespec it is given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) };
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
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
    mut names: Option<Names>,
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
    let mut heading_soon: Option<(i32, i32)>;
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
    // Where we are, as the canvases should show it, the route, how far
    // along it, and day or night.
    let mut position: Option<Marker> = None;
    let mut route: Option<Route> = None;
    let mut cameras: Option<CameraMarks> = None;
    let mut progress: Option<Progress> = None;
    let mut night = false;
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
                let new_cameras = match (&inner.cameras, &cameras) {
                    (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
                    (None, None) => false,
                    _ => true,
                };
                if new_cameras {
                    // Every picture has the old ones (or none).
                    cameras = inner.cameras.clone();
                    held.fill(None);
                    up_held.fill(None);
                    ahead = None;
                    shown = None;
                }
                heading_soon = inner.heading_soon;
                if inner.heading != heading {
                    heading = inner.heading.filter(|_| !ups.is_empty());
                    shown = None;
                }
                if inner.night != night {
                    night = inner.night;
                    if tiles.set_night(night) {
                        // Every picture has the other tiles.
                        held.fill(None);
                        up_held.fill(None);
                        ahead = None;
                        shown = None;
                    }
                }
                if inner.progress != progress {
                    progress = inner.progress;
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
        let Some(mut want) = want else {
            // Nothing new to show: a step towards the canvas ahead.
            if let Some(layout) = ahead
                && !tiles.prepare(&layout)
            {
                // Every tile kept: the canvas composed from them.
                ahead = None;
                let started = Instant::now();
                if let Some(i) = compose_free(
                    &mut tiles,
                    names.as_mut(),
                    &mut canvases,
                    &mut held,
                    &recent,
                    &layout,
                    baked,
                    route.as_deref(),
                    cameras.as_deref(),
                    progress,
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
                    .find_map(|i| {
                        let u = up_held[i]?;
                        fresh(u.progress, progress, route.is_some(), u.zoom).then_some(())?;
                        u.window(&want, h, size, panel).map(|w| (i, w))
                    })
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
                    let mut cost = UpCost::default();
                    // The frame shown, moved along meanwhile: with the
                    // marker on its sprite, the pictures need not change.
                    let meanwhile = recent_up[0]
                        .filter(|_| sprites.is_some())
                        .and_then(|j| up_held[j].map(|u| (j, u)));
                    let job = UpJob {
                        view: want,
                        // Drawn for the way up when it will be shown.
                        h: heading_soon.map_or(h, unit_up),
                        marker: baked,
                        progress,
                        anchor: anchor_on(panel, window),
                        top,
                    };
                    let drawn = match meanwhile {
                        Some((j, u)) => {
                            let (to, on) = pair_mut(&mut ups, i, j);
                            let mut moving = Moving {
                                shared,
                                display,
                                sprites: &mut sprites,
                                canvas: on,
                                held: u,
                                route: route.is_some(),
                                size,
                                panel,
                                last: Some(want),
                                any: false,
                            };
                            thread::scope(|scope| {
                                let names = names.as_mut();
                                let tiles = &mut tiles;
                                let (route, cameras) = (route.as_deref(), cameras.as_deref());
                                let cost = &mut cost;
                                let turner = thread::Builder::new()
                                    .name("map-turn".into())
                                    .spawn_scoped(scope, move || {
                                        // Below the frames shown.
                                        // SAFETY: nice() changes only this
                                        // thread's priority on Linux.
                                        unsafe { libc::nice(5) };
                                        let held = render_up(
                                            tiles,
                                            names,
                                            &mut surface(to),
                                            &job.view,
                                            job.h,
                                            job.marker,
                                            route,
                                            cameras,
                                            job.progress,
                                            job.anchor,
                                            job.top,
                                            cost,
                                        );
                                        shared.wake.notify_one();
                                        held
                                    });
                                match turner {
                                    Ok(t) => {
                                        while !t.is_finished() {
                                            if moving.step(&mut up_stats) {
                                                recent_up =
                                                    [Some(j), recent_up[0].filter(|&r| r != j)];
                                            }
                                        }
                                        if !moving.any {
                                            up_stats.blind += 1;
                                            if u.zoom != job.view.zoom {
                                                up_stats.blind_zoom += 1;
                                            }
                                        }
                                        t.join().ok()
                                    }
                                    Err(e) => {
                                        log::warn!("map: no thread to turn the map on: {e}");
                                        None
                                    }
                                }
                            })
                        }
                        None => Some({
                            up_stats.blind += 1;
                            render_up(
                                &mut tiles,
                                names.as_mut(),
                                &mut surface(&mut ups[i]),
                                &job.view,
                                job.h,
                                job.marker,
                                route.as_deref(),
                                cameras.as_deref(),
                                job.progress,
                                job.anchor,
                                job.top,
                                &mut cost,
                            )
                        }),
                    };
                    let Some(drawn) = drawn else {
                        break;
                    };
                    up_held[i] = Some(drawn);
                    last_turn = Some(Instant::now());
                    up_stats.turned(started.elapsed(), cost);
                    // Shown at once where the view is now, unless it went
                    // on further than the frame reaches: then the loop
                    // again, with what is wanted now.
                    let now = {
                        let inner = shared.lock();
                        (inner.want, inner.heading, inner.position, inner.progress)
                    };
                    let h_now = now.1.map_or(h, unit_up);
                    match drawn.window_within(&now.0, h_now, size, panel, UP_LOOSE) {
                        Some(w) => {
                            want = now.0;
                            position = now.2;
                            progress = now.3;
                            (i, w)
                        }
                        None => {
                            shown = None;
                            continue;
                        }
                    }
                }
            };
            up_stats.frame();
            let held_i = up_held[i];
            if let Some(u) = held_i
                && show_up(display, &mut sprites, &ups[i], &u, (x, y), position)
            {
                up_stats.shown();
                recent_up = [Some(i), recent_up[0].filter(|&r| r != i)];
                shared.lock().shown = true;
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
                fresh(h.progress, progress, route.is_some(), h.layout.zoom).then_some(())?;
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
                    // To even pixels, as the tiles are put back.
                    let rect = marker_rect(&h.layout, old);
                    let rect = (
                        rect.0 & !1,
                        rect.1 & !1,
                        (rect.2 + 1) & !1,
                        (rect.3 + 1) & !1,
                    );
                    tiles.compose_rect(&h.layout, canvas.pixels(), pitch, chroma, rect);
                    if let Some(line) = route.as_deref() {
                        draw_route(
                            canvas.pixels(),
                            pitch,
                            chroma,
                            size,
                            &h.layout,
                            rect,
                            line,
                            progress,
                        );
                    }
                    if let Some(n) = names.as_mut() {
                        draw_names(n, &mut surface(canvas), &h.layout, rect, tiles.night());
                    }
                    if let Some(c) = cameras.as_deref() {
                        draw_cameras(&mut surface(canvas), &h.layout, rect, c);
                    }
                }
                draw_marker(canvas.pixels(), pitch, chroma, size, &h.layout, baked);
                held[i] = Some(Held {
                    layout: h.layout,
                    marker: baked,
                    progress,
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
                    names.as_mut(),
                    &mut canvases,
                    &mut held,
                    &recent,
                    &layout,
                    baked,
                    route.as_deref(),
                    cameras.as_deref(),
                    progress,
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

/// A heading-up frame to turn: the view, the way up, the marker drawn in
/// (none with the sprite), how far along the route, where the view's
/// point goes and the first row drawn.
#[derive(Clone, Copy)]
struct UpJob {
    view: View,
    h: (i64, i64),
    marker: Option<Marker>,
    progress: Option<Progress>,
    anchor: (i64, i64),
    top: u32,
}

/// Two canvases of `all`, the first to draw on and the second to show.
fn pair_mut(all: &mut [Canvas], draw: usize, show: usize) -> (&mut Canvas, &Canvas) {
    assert_ne!(draw, show, "a canvas drawn on is not shown");
    if draw < show {
        let (a, b) = all.split_at_mut(show);
        (&mut a[draw], &b[0])
    } else {
        let (a, b) = all.split_at_mut(draw);
        (&mut b[0], &a[show])
    }
}

/// The frame shown, moved along while another is turned: each view the
/// drive wants shown on it while it holds the window and is turned
/// within `UP_LOOSE` of the way up.
struct Moving<'a> {
    shared: &'a Shared,
    display: &'a Display,
    sprites: &'a mut Option<Sprites>,
    canvas: &'a Canvas,
    held: UpHeld,
    route: bool,
    size: (u32, u32),
    panel: (u32, u32),
    last: Option<View>,
    /// Something was shown on it.
    any: bool,
}

impl Moving<'_> {
    /// Waits for the next view (or the turn's end, or a while), and shows
    /// it when this frame can. True when a picture was shown.
    fn step(&mut self, stats: &mut UpStats) -> bool {
        let (want, heading, position, progress) = {
            let inner = self.shared.lock();
            let (inner, _) = self
                .shared
                .wake
                .wait_timeout(inner, Duration::from_millis(40))
                .unwrap_or_else(PoisonError::into_inner);
            if inner.stop {
                return false;
            }
            (inner.want, inner.heading, inner.position, inner.progress)
        };
        if self.last == Some(want) {
            return false;
        }
        let Some(h) = heading.map(unit_up) else {
            return false;
        };
        let u = self.held;
        if !fresh_within(u.progress, progress, self.route, u.zoom, ROUTE_LAG_LOOSE) {
            return false;
        }
        let Some(w) = u.window_within(&want, h, self.size, self.panel, UP_LOOSE) else {
            return false;
        };
        stats.frame();
        if show_up(self.display, self.sprites, self.canvas, &u, w, position) {
            stats.shown();
            self.last = Some(want);
            self.any = true;
            self.shared.lock().shown = true;
            return true;
        }
        false
    }
}

/// Show the heading-up frame `canvas` (holding `u`) with its window at
/// `(x, y)`, the marker on its sprite; a pending flip tried again a
/// moment later. True when shown.
fn show_up(
    display: &Display,
    sprites: &mut Option<Sprites>,
    canvas: &Canvas,
    u: &UpHeld,
    (x, y): (u32, u32),
    position: Option<Marker>,
) -> bool {
    let sprite = match (sprites.as_mut(), position) {
        (Some(sp), Some(m)) => {
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
    let mut tries = 0;
    loop {
        match display.show_canvas_with(canvas, x, y, sprite) {
            Show::Shown => return true,
            Show::Busy if tries < 50 => {
                tries += 1;
                thread::sleep(BUSY_WAIT);
            }
            Show::Busy | Show::Failed => return false,
        }
    }
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
    /// Of the turns: the tiles found (decoded when not kept), turned, and
    /// the route, names and signs drawn over them.
    cost: UpCost,
    /// Between two pictures shown: the longest, and how many were longer
    /// than `LATE_MS` (a picture missed).
    gap_most: Duration,
    late: u32,
    /// Turns with nothing shown meanwhile, and of those, at another zoom.
    blind: u32,
    blind_zoom: u32,
    last_shown: Option<Instant>,
    since: Instant,
}

/// What a turned frame cost, by part.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct UpCost {
    pub tiles: Duration,
    pub rotate: Duration,
    pub draw: Duration,
}

impl UpStats {
    fn new() -> Self {
        Self {
            frames: 0,
            turned: 0,
            took: Duration::ZERO,
            most: Duration::ZERO,
            cost: UpCost::default(),
            gap_most: Duration::ZERO,
            late: 0,
            blind: 0,
            blind_zoom: 0,
            last_shown: None,
            since: Instant::now(),
        }
    }

    fn turned(&mut self, took: Duration, cost: UpCost) {
        self.turned += 1;
        self.took += took;
        self.most = self.most.max(took);
        self.cost.tiles += cost.tiles;
        self.cost.rotate += cost.rotate;
        self.cost.draw += cost.draw;
    }

    /// A picture reached the panel.
    fn shown(&mut self) {
        let now = Instant::now();
        if let Some(last) = self.last_shown {
            let gap = now - last;
            self.gap_most = self.gap_most.max(gap);
            if gap > Duration::from_millis(LATE_MS) {
                self.late += 1;
            }
        }
        self.last_shown = Some(now);
    }

    fn frame(&mut self) {
        self.frames += 1;
        if self.since.elapsed() >= UP_STATS_EVERY {
            let each = |d: Duration| d.as_millis() / u128::from(self.turned.max(1));
            log::info!(
                "map: {} frames heading up in {} s, {} turned, {} ms each ({} tiles, {} turning, {} drawing over), {} ms at most; {} gaps over {LATE_MS} ms, {} ms the longest; {} turned with nothing shown meanwhile, {} of them for another zoom",
                self.frames,
                self.since.elapsed().as_secs(),
                self.turned,
                each(self.took),
                each(self.cost.tiles),
                each(self.cost.rotate),
                each(self.cost.draw),
                self.most.as_millis(),
                self.late,
                self.gap_most.as_millis(),
                self.blind,
                self.blind_zoom
            );
            let last = self.last_shown;
            *self = Self::new();
            self.last_shown = last;
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

/// Two pictures further apart than this, a refresh and a half at 60 Hz,
/// missed one.
const LATE_MS: u64 = 25;
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
/// While a frame is turned anew, the one shown may be this far from the
/// way up and is moved along still: the sine of 8 degrees, times 2^32.
const UP_LOOSE: i64 = 597_000_000;
/// Standing still, the drive is looked at every this many refreshes: a
/// start is seen within a fifteenth of a second at 60 Hz.
const STILL_EVERY: u32 = 4;
/// A frame is turned for the way up this many milliseconds on: what
/// turning one takes on the board, and the refresh it waits for.
const SOON_MS: u64 = 60;

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
pub(crate) struct UpHeld {
    zoom: u8,
    p: (i64, i64),
    h: (i64, i64),
    anchor: (i64, i64),
    top: u32,
    progress: Option<Progress>,
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
        self.window_within(view, h, size, panel, UP_TOLERANCE)
    }

    /// How far its way up is from `h`: the sine of the angle, times 2^32;
    /// None when it is a right angle or more.
    fn off(&self, h: (i64, i64)) -> Option<i64> {
        let cross = self.h.0 * h.1 - self.h.1 * h.0;
        let dot = self.h.0 * h.0 + self.h.1 * h.1;
        (dot > 0).then_some(cross.abs())
    }

    /// As `window`, for a frame turned up to `tolerance` from `h`.
    fn window_within(
        &self,
        view: &View,
        h: (i64, i64),
        size: (u32, u32),
        panel: (u32, u32),
        tolerance: i64,
    ) -> Option<(u32, u32)> {
        if view.zoom != self.zoom || self.off(h).is_none_or(|o| o > tolerance) {
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

/// A canvas to draw on.
fn surface(c: &mut Canvas) -> Surface<'_> {
    let (pitch, chroma, size) = (c.pitch(), c.chroma_offset(), (c.width(), c.height()));
    Surface {
        pixels: c.pixels(),
        pitch,
        chroma,
        size,
    }
}

/// A heading-up frame of `view` on `out`, rows `top` (made even) down:
/// the tiles turned so that `h` points up, the view's point at `anchor`;
/// then the route, the names over it, and the marker (if it is to be
/// drawn in), turned the same way. What the frame then holds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_up(
    tiles: &mut Tiles,
    names: Option<&mut Names>,
    out: &mut Surface<'_>,
    view: &View,
    h: (i64, i64),
    marker: Option<Marker>,
    route: Option<&[(i64, i64)]>,
    cameras: Option<&[(i64, i64)]>,
    progress: Option<Progress>,
    anchor: (i64, i64),
    top: u32,
    cost: &mut UpCost,
) -> UpHeld {
    let size = out.size;
    let (pitch, chroma) = (out.pitch, out.chroma);
    let top = top & !1;
    let held = UpHeld {
        zoom: view.zoom,
        p: (fine(view.zoom, view.x), fine(view.zoom, view.y)),
        h,
        anchor,
        top,
        progress,
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
        let clock = Instant::now();
        let grid = tiles.grid(view.zoom, (tx0, ty0), cols, rows);
        cost.tiles = clock.elapsed();
        let clock = Instant::now();
        let origin = (p.0 - tx0 * tile, p.1 - ty0 * tile);
        // From row `top`: its luma, and its chroma's place from there.
        let skip = top as usize * pitch;
        nv12::rotate(
            &grid,
            &mut out.pixels[skip..],
            pitch,
            chroma - (top as usize / 2) * pitch,
            (size.0, size.1 - top),
            origin,
            (anchor.0, anchor.1 - t),
            across,
            down,
        );
        cost.rotate = clock.elapsed();
    }
    let drawing = Instant::now();
    if let Some(line) = route {
        let points: Vec<(i64, i64)> = line.iter().map(|&q| held.sixteenth(q)).collect();
        let split = progress.map(|q| (q.step, held.sixteenth(q.at)));
        draw_split(
            out.pixels,
            pitch,
            chroma,
            size,
            (0, t, w, ht),
            &points,
            split,
        );
    }
    // The part drawn, on the map, in world pixels.
    let area = {
        let s = i32::from(WORLD_Z - view.zoom.min(WORLD_Z)) - 16;
        let world = |v: i64| if s >= 0 { v << s } else { v >> -s };
        let xs = corners.iter().map(|c| c.0);
        let ys = corners.iter().map(|c| c.1);
        (
            world(xs.clone().min().unwrap_or(0)),
            world(ys.clone().min().unwrap_or(0)),
            world(xs.max().unwrap_or(0)),
            world(ys.max().unwrap_or(0)),
        )
    };
    if let Some(n) = names {
        n.draw(
            out,
            (0, t, w, ht),
            view.zoom,
            area,
            tiles.night(),
            |q| held.sixteenth(q),
            |m| held.turn(m),
        );
    }
    if let Some(c) = cameras {
        signs(out, (0, t, w, ht), view.zoom, area, c, |q| {
            held.sixteenth(q)
        });
    }
    match marker {
        Some(Marker::Dot(x, y)) => {
            let at = held.sixteenth((x, y));
            nv12::dot(out.pixels, pitch, chroma, size, (at.0 >> 4, at.1 >> 4));
        }
        Some(Marker::Arrow(x, y, m)) => {
            let at = held.sixteenth((x, y));
            let turned = held.turn((i64::from(m.0), i64::from(m.1)));
            nv12::arrow(
                out.pixels,
                pitch,
                chroma,
                size,
                (at.0 >> 4, at.1 >> 4),
                (turned.0 as i32, turned.1 as i32),
            );
        }
        None => {}
    }
    cost.draw = drawing.elapsed();
    held
}

/// The route on a canvas, only inside `clip` (its pixels), grey up to
/// `progress`.
#[allow(clippy::too_many_arguments)]
fn draw_route(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    layout: &Layout,
    clip: (i64, i64, i64, i64),
    line: &[(i64, i64)],
    progress: Option<Progress>,
) {
    let points: Vec<(i64, i64)> = line.iter().map(|&p| layout.sixteenth(p)).collect();
    let split = progress.map(|p| (p.step, layout.sixteenth(p.at)));
    draw_split(dst, pitch, chroma, size, clip, &points, split);
}

/// The route's points (1/16 px of the canvas), grey behind the split: the
/// points before `step`, then `at`.
fn draw_split(
    dst: &mut [u8],
    pitch: usize,
    chroma: usize,
    size: (u32, u32),
    clip: (i64, i64, i64, i64),
    points: &[(i64, i64)],
    split: Option<(usize, (i64, i64))>,
) {
    match split {
        Some((step, at)) if 0 < step && step < points.len() => {
            let mut behind = points[..step].to_vec();
            behind.push(at);
            let mut ahead = vec![at];
            ahead.extend_from_slice(&points[step..]);
            nv12::route_behind(dst, pitch, chroma, size, clip, &behind);
            nv12::route(dst, pitch, chroma, size, clip, &ahead);
        }
        _ => nv12::route(dst, pitch, chroma, size, clip, points),
    }
}

/// The names on a north-up canvas of `layout`, only inside `clip` (its
/// pixels), along the ways they have on the map.
pub(crate) fn draw_names(
    names: &mut Names,
    out: &mut Surface<'_>,
    layout: &Layout,
    clip: (i64, i64, i64, i64),
    night: bool,
) {
    let s = WORLD_Z - layout.zoom.min(WORLD_Z);
    let world = |px: i64, tile: i64| (px + tile * TILE) << s;
    let area = (
        world(clip.0, layout.tx),
        world(clip.1, layout.ty),
        world(clip.2, layout.tx),
        world(clip.3, layout.ty),
    );
    names.draw(
        out,
        clip,
        layout.zoom,
        area,
        night,
        |p| layout.sixteenth(p),
        |m| m,
    );
}

/// The speed cameras on a north-up canvas of `layout`, only inside `clip`
/// (its pixels).
fn draw_cameras(
    out: &mut Surface<'_>,
    layout: &Layout,
    clip: (i64, i64, i64, i64),
    cameras: &[(i64, i64)],
) {
    let s = WORLD_Z - layout.zoom.min(WORLD_Z);
    let world = |px: i64, tile: i64| (px + tile * TILE) << s;
    let area = (
        world(clip.0, layout.tx),
        world(clip.1, layout.ty),
        world(clip.2, layout.tx),
        world(clip.3, layout.ty),
    );
    signs(out, clip, layout.zoom, area, cameras, |p| {
        layout.sixteenth(p)
    });
}

/// The speed cameras within `area` (world pixels, widened by a sign's
/// reach) as signs, each where `place` puts it (1/16 px), inside `clip`;
/// none further out than [`CAMERA_ZOOM`].
fn signs(
    out: &mut Surface<'_>,
    clip: (i64, i64, i64, i64),
    zoom: u8,
    area: (i64, i64, i64, i64),
    cameras: &[(i64, i64)],
    place: impl Fn((i64, i64)) -> (i64, i64),
) {
    if zoom < CAMERA_ZOOM {
        return;
    }
    let reach = nv12::SIGN_REACH << (WORLD_Z - zoom.min(WORLD_Z));
    let (x0, y0, x1, y1) = (
        area.0 - reach,
        area.1 - reach,
        area.2 + reach,
        area.3 + reach,
    );
    let first = cameras.partition_point(|c| c.0 < x0);
    for &c in cameras[first..].iter().take_while(|c| c.0 <= x1) {
        if c.1 < y0 || c.1 > y1 {
            continue;
        }
        let at = place(c);
        nv12::camera_sign(
            out.pixels,
            out.pitch,
            out.chroma,
            out.size,
            clip,
            (at.0 >> 4, at.1 >> 4),
        );
    }
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
/// replaced, with the route, the names and the marker at `position`, and
/// say which; None when there is none such.
#[allow(clippy::too_many_arguments)]
fn compose_free(
    tiles: &mut Tiles,
    names: Option<&mut Names>,
    canvases: &mut [Canvas],
    held: &mut [Option<Held>],
    recent: &[Option<usize>; 2],
    layout: &Layout,
    position: Option<Marker>,
    route: Option<&[(i64, i64)]>,
    cameras: Option<&[(i64, i64)]>,
    progress: Option<Progress>,
) -> Option<usize> {
    let index = (0..canvases.len()).find(|i| !recent.contains(&Some(*i)))?;
    let canvas = &mut canvases[index];
    let (pitch, chroma) = (canvas.pitch(), canvas.chroma_offset());
    let size = (canvas.width(), canvas.height());
    held[index] = None;
    tiles.compose(layout, canvas.pixels(), pitch, chroma);
    if let Some(line) = route {
        let whole = (0, 0, i64::from(size.0), i64::from(size.1));
        draw_route(
            canvas.pixels(),
            pitch,
            chroma,
            size,
            layout,
            whole,
            line,
            progress,
        );
    }
    if let Some(n) = names {
        let whole = (0, 0, i64::from(size.0), i64::from(size.1));
        draw_names(n, &mut surface(canvas), layout, whole, tiles.night());
    }
    if let Some(c) = cameras {
        let whole = (0, 0, i64::from(size.0), i64::from(size.1));
        draw_cameras(&mut surface(canvas), layout, whole, c);
    }
    draw_marker(canvas.pixels(), pitch, chroma, size, layout, position);
    held[index] = Some(Held {
        layout: *layout,
        marker: position,
        progress,
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
            progress: None,
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
