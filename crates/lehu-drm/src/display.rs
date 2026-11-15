// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The device: opening the card, choosing the planes, and every commit.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use drm::buffer::{DrmFourcc, DrmModifier, Handle as GemHandle, PlanarBuffer};
use drm::control::{
    self, Device as ControlDevice, Event, FbCmd2Flags, PlaneType, ResourceHandle, connector,
    framebuffer, plane, property,
};
use drm::{ClientCapability, Device as _, DriverCapability};

use crate::band::{self, Band, Rect};
use crate::cache::{self, Entry, FbCache, Gem};
use crate::error::Error;
use crate::frame::{Frame, MAX_PLANES};
use crate::request::Request;

/// Framebuffers kept at most; a pool is a dozen, the camera four.
const CACHE_CAPACITY: usize = 32;

/// A commit that returns before the flip, with an event on the descriptor
/// when the flip is done.
const NONBLOCK: u32 = drm_ffi::DRM_MODE_ATOMIC_NONBLOCK | drm_ffi::DRM_MODE_PAGE_FLIP_EVENT;
const ALLOW_MODESET: u32 = drm_ffi::DRM_MODE_ATOMIC_ALLOW_MODESET;

/// What became of a picture handed to `show_video` or `show_camera`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Show {
    /// Queued for scanout. The buffer stays on the plane until the next
    /// `Shown`: keep it alive until then.
    Shown,
    /// The previous flip is still pending (or the camera holds the plane);
    /// nothing changed. Offer the newest frame again after the next flip.
    Busy,
    /// Not shown, and trying again would not help.
    Failed,
}

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for Card {}
impl ControlDevice for Card {}

impl Gem for Card {
    fn rm_fb(&self, fb: u32) {
        if let Some(h) = control::from_u32::<framebuffer::Handle>(fb) {
            let _ = self.destroy_framebuffer(h);
        }
    }

    fn close_handle(&self, handle: u32) {
        if let Some(h) = control::from_u32::<GemHandle>(handle) {
            let _ = self.close_buffer(h);
        }
    }
}

/// The ids of a plane's atomic properties.
#[derive(Clone, Copy)]
struct PlaneProps {
    fb: u32,
    crtc: u32,
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    crtc_x: u32,
    crtc_y: u32,
    crtc_w: u32,
    crtc_h: u32,
    /// Not every driver has it.
    zpos: Option<u32>,
    /// `COLOR_RANGE`, where the driver has it: its id, and the values for
    /// "YCbCr limited range" and "YCbCr full range".
    color_range: Option<(u32, u64, u64)>,
}

/// The overlay plane the UI goes on, fixed at open.
struct UiPlane {
    id: u32,
    props: PlaneProps,
    format: DrmFourcc,
    bpp: u32,
}

/// The buffers of a framebuffer, as the ioctl wants them.
struct Planar {
    size: (u32, u32),
    format: DrmFourcc,
    modifier: Option<DrmModifier>,
    pitches: [u32; 4],
    handles: [Option<GemHandle>; 4],
    offsets: [u32; 4],
}

impl PlanarBuffer for Planar {
    fn size(&self) -> (u32, u32) {
        self.size
    }
    fn format(&self) -> DrmFourcc {
        self.format
    }
    fn modifier(&self) -> Option<DrmModifier> {
        self.modifier
    }
    fn pitches(&self) -> [u32; 4] {
        self.pitches
    }
    fn handles(&self) -> [Option<GemHandle>; 4] {
        self.handles
    }
    fn offsets(&self) -> [u32; 4] {
        self.offsets
    }
}

/// A dumb buffer with its framebuffer, mapped. Write-combined memory:
/// written by rows, never read.
struct Dumb {
    handle: u32,
    pitch: u32,
    size: usize,
    map: NonNull<u8>,
    fb: framebuffer::Handle,
}

// SAFETY: the mapping is shared memory that only `Dumb`'s own methods touch,
// and they run under the display's mutex.
unsafe impl Send for Dumb {}

impl Dumb {
    fn create(
        card: &Card,
        width: u32,
        height: u32,
        format: DrmFourcc,
        bpp: u32,
    ) -> Result<Self, Error> {
        let fd = card.as_fd();
        // NV12: the luma's rows, then the interleaved chroma's at half the
        // height, in one buffer of one-byte "pixels".
        let nv12 = format == DrmFourcc::Nv12;
        let rows = if nv12 { height + height / 2 } else { height };
        let created = drm_ffi::mode::dumbbuffer::create(fd, width, rows, bpp * 8, 0)
            .map_err(|e| Error::Io("CREATE_DUMB", e))?;
        let handle = created.handle;
        let destroy = || {
            let _ = drm_ffi::mode::dumbbuffer::destroy(fd, handle);
        };
        let size = match usize::try_from(created.size) {
            Ok(s) if s > 0 => s,
            _ => {
                destroy();
                return Err(Error::Io("CREATE_DUMB", io::Error::other("bad size")));
            }
        };
        let mapped = match drm_ffi::mode::dumbbuffer::map(fd, handle, 0, 0) {
            Ok(m) => m,
            Err(e) => {
                destroy();
                return Err(Error::Io("MAP_DUMB", e));
            }
        };
        // SAFETY: a plain mapping of the buffer's fake offset on our own
        // descriptor; the result is checked before use. The offset is 64-bit
        // even on a 32-bit system, hence mmap64.
        let ptr = unsafe {
            libc::mmap64(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                mapped.offset as libc::off64_t,
            )
        };
        let map = if ptr == libc::MAP_FAILED {
            None
        } else {
            NonNull::new(ptr.cast::<u8>())
        };
        let Some(map) = map else {
            let e = io::Error::last_os_error();
            destroy();
            return Err(Error::Io("mmap dumb", e));
        };
        let luma = created.pitch as usize * height as usize;
        // SAFETY: `size` bytes just mapped, ours alone. Black to begin with:
        // in NV12 that is luma 16 and neutral chroma, not zeros (green).
        unsafe {
            if nv12 && luma <= size {
                std::ptr::write_bytes(map.as_ptr(), 16, luma);
                std::ptr::write_bytes(map.as_ptr().add(luma), 128, size - luma);
            } else {
                std::ptr::write_bytes(map.as_ptr(), 0, size);
            }
        }
        let gem = control::from_u32(handle);
        let planar = if nv12 {
            Planar {
                size: (width, height),
                format,
                modifier: None,
                pitches: [created.pitch, created.pitch, 0, 0],
                handles: [gem, gem, None, None],
                offsets: [0, luma as u32, 0, 0],
            }
        } else {
            Planar {
                size: (width, height),
                format,
                modifier: None,
                pitches: [created.pitch, 0, 0, 0],
                handles: [gem, None, None, None],
                offsets: [0; 4],
            }
        };
        match card.add_planar_framebuffer(&planar, FbCmd2Flags::empty()) {
            Ok(fb) => Ok(Self {
                handle,
                pitch: created.pitch,
                size,
                map,
                fb,
            }),
            Err(e) => {
                // SAFETY: the mapping made above, unmapped once.
                let _ = unsafe { libc::munmap(ptr, size) };
                destroy();
                Err(Error::Io("ADDFB2 dumb", e))
            }
        }
    }

    fn destroy(self, card: &Card) {
        let _ = card.destroy_framebuffer(self.fb);
        // SAFETY: the mapping made in `create`, unmapped once, here.
        let _ = unsafe { libc::munmap(self.map.as_ptr().cast(), self.size) };
        let _ = drm_ffi::mode::dumbbuffer::destroy(card.as_fd(), self.handle);
    }

    /// Copy `rows` rows of `row_bytes` from `src` (rows `src_pitch` apart)
    /// to the buffer, starting `x_bytes` into row `y`. False when either
    /// side does not hold the rows.
    fn write_rows(
        &self,
        x_bytes: usize,
        y: usize,
        rows: usize,
        row_bytes: usize,
        src: &[u8],
        src_pitch: usize,
    ) -> bool {
        let pitch = self.pitch as usize;
        let Some(start) = y.checked_mul(pitch).and_then(|v| v.checked_add(x_bytes)) else {
            return false;
        };
        let (Some(need), Some(have)) = (
            band::span(rows, row_bytes, pitch),
            band::span(rows, row_bytes, src_pitch),
        ) else {
            return false;
        };
        if have > src.len() || start.checked_add(need).is_none_or(|end| end > self.size) {
            return false;
        }
        for r in 0..rows {
            // SAFETY: both ranges were checked against their buffers above,
            // and a heap slice never overlaps the mapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr().add(r * src_pitch),
                    self.map.as_ptr().add(start + r * pitch),
                    row_bytes,
                );
            }
        }
        true
    }
}

/// Warnings given once: a failure that repeats every frame must not flood
/// the log.
#[derive(Default)]
struct Warned {
    frame: bool,
    import: bool,
    video_commit: bool,
    ui_commit: bool,
}

struct State {
    request: Request,
    /// The connector, CRTC and mode are set; later commits only flip.
    modeset_done: bool,
    /// The UI's buffer; None without an overlay plane.
    ui: Option<Dumb>,
    /// What the UI plane covers.
    band: Band,
    cache: FbCache,
    /// The camera owns the video plane: the decoder's frames wait.
    video_held: bool,
    /// A map canvas on the video plane is full-range YCbCr (the JPEG
    /// tiles as the engine decodes them), told to the plane when it has
    /// `COLOR_RANGE`; the phone's video and the camera are limited range.
    canvas_full_range: bool,
    /// Black buffer for the video plane. Some drivers refuse to enable the
    /// CRTC with no primary plane, so a UI-only modeset (the home screen
    /// before any video) may need the primary enabled with something. Freed
    /// once real video has replaced it on scanout, made again if the UI
    /// ever needs it.
    placeholder: Option<Dumb>,
    /// Video commits since the placeholder left the plane.
    placeholder_live: u32,
    /// The sprite plane shows a picture.
    sprite_on: bool,
    /// The driver refused the sprite plane once: never asked again.
    sprite_refused: bool,
    /// The marker the map wants on the cursor plane, shown or, while the
    /// UI covers it, not: the cursor is above every plane, the UI's too,
    /// so a UI window over it takes it down and its going brings it back.
    cursor_want: Option<CursorAt>,
    /// The framebuffer the cursor plane's committed state holds, 0 off.
    /// The driver turns a framebuffer into the cursor's palette and
    /// pattern only when the plane gets a new one.
    cursor_fb: u32,
    /// The driver refused the cursor plane once: never asked again.
    cursor_refused: bool,
    warned: Warned,
}

/// A cursor canvas, `size` a side, at (`x`, `y`) of the panel, its top
/// left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorAt {
    fb: u32,
    size: u32,
    x: i32,
    y: i32,
}

/// The cursor plane's smallest side, in pixels; 64 where the driver says
/// it takes that (`cursor_max`). The F1C200s's pattern memory holds 16
/// colours at 32 x 32 (4 at 64 x 32, 2 at 64 x 64).
pub const CURSOR_SIZE: u32 = 32;

/// The panel. Shared between threads through an `Arc`: every method takes
/// `&self`.
pub struct Display {
    card: Card,
    width: u32,
    height: u32,
    width_mm: u32,
    height_mm: u32,
    connector: u32,
    crtc: u32,
    mode_blob: u64,
    conn_crtc: u32,
    crtc_mode: u32,
    crtc_active: u32,
    video_plane: u32,
    video_props: PlaneProps,
    /// The video plane takes RGB565: the placeholder's format, else XRGB8888.
    video_rgb565: bool,
    ui_plane: Option<UiPlane>,
    /// A plane with per-pixel alpha between the video and the UI (the
    /// map's marker): its id and properties.
    sprite_plane: Option<(u32, PlaneProps)>,
    /// The cursor plane, above all the others (the map's marker, when
    /// there is one): its id and properties. None with LEHU_MAP_CURSOR=0.
    cursor_plane: Option<(u32, PlaneProps)>,
    /// The largest square the cursor takes: 32 or 64.
    cursor_max: u32,
    ui_pitch: u32,
    /// Frames of the phone's video presented so far; read lock-free.
    video_frames: AtomicU32,
    state: Mutex<State>,
    /// LEHU_SHOT_DIR in the environment: a folder where, whenever a file
    /// `take` appears in it, the next canvas shown is written out with the
    /// sprite and the UI over it (for screenshots of the map, which no
    /// tool reads off the video plane; scripts/map-shot.py composes them).
    shot_dir: Option<std::path::PathBuf>,
}

fn find_prop(card: &Card, handle: impl ResourceHandle, name: &str) -> Option<property::Handle> {
    let set = card.get_properties(handle).ok()?;
    let (ids, _) = set.as_props_and_values();
    ids.iter().copied().find(|&id| {
        card.get_property(id)
            .is_ok_and(|info| info.name().to_bytes() == name.as_bytes())
    })
}

fn prop(card: &Card, handle: impl ResourceHandle, name: &'static str) -> Result<u32, Error> {
    find_prop(card, handle, name)
        .map(u32::from)
        .ok_or(Error::Property(name))
}

/// An enum property of `handle` by `name`, with the raw values of two of
/// its choices by their names; None unless all three are there.
fn enum_prop(
    card: &Card,
    handle: impl ResourceHandle,
    name: &str,
    a: &str,
    b: &str,
) -> Option<(u32, u64, u64)> {
    let id = find_prop(card, handle, name)?;
    let info = card.get_property(id).ok()?;
    let property::ValueType::Enum(values) = info.value_type() else {
        return None;
    };
    let (raw, names) = values.values();
    let of = |want: &str| {
        names
            .iter()
            .zip(raw)
            .find(|(e, _)| e.name().to_bytes() == want.as_bytes())
            .map(|(_, &v)| v)
    };
    Some((u32::from(id), of(a)?, of(b)?))
}

fn plane_props(card: &Card, plane: plane::Handle) -> Result<PlaneProps, Error> {
    Ok(PlaneProps {
        fb: prop(card, plane, "FB_ID")?,
        crtc: prop(card, plane, "CRTC_ID")?,
        src_x: prop(card, plane, "SRC_X")?,
        src_y: prop(card, plane, "SRC_Y")?,
        src_w: prop(card, plane, "SRC_W")?,
        src_h: prop(card, plane, "SRC_H")?,
        crtc_x: prop(card, plane, "CRTC_X")?,
        crtc_y: prop(card, plane, "CRTC_Y")?,
        crtc_w: prop(card, plane, "CRTC_W")?,
        crtc_h: prop(card, plane, "CRTC_H")?,
        zpos: find_prop(card, plane, "zpos").map(u32::from),
        color_range: enum_prop(
            card,
            plane,
            "COLOR_RANGE",
            "YCbCr limited range",
            "YCbCr full range",
        ),
    })
}

fn plane_type(card: &Card, plane: plane::Handle) -> Option<PlaneType> {
    let set = card.get_properties(plane).ok()?;
    let (ids, values) = set.as_props_and_values();
    let value = ids
        .iter()
        .zip(values)
        .find(|(id, _)| {
            card.get_property(**id)
                .is_ok_and(|info| info.name().to_bytes() == b"type")
        })
        .map(|(_, v)| *v)?;
    [PlaneType::Overlay, PlaneType::Primary, PlaneType::Cursor]
        .into_iter()
        .find(|t| *t as u64 == value)
}

/// The cursor at `a` shows on a `width` x `height` panel: some of it on
/// the panel, none of it under the UI window `ui`.
fn cursor_visible(a: CursorAt, width: u32, height: u32, ui: Option<Rect>) -> bool {
    let (x0, y0) = (i64::from(a.x), i64::from(a.y));
    let (x1, y1) = (x0 + i64::from(a.size), y0 + i64::from(a.size));
    let on_panel = x1 > 0 && y1 > 0 && x0 < i64::from(width) && y0 < i64::from(height);
    let under_ui = ui.is_some_and(|r| {
        x1 > i64::from(r.x0) && x0 < i64::from(r.x1) && y1 > i64::from(r.y0) && y0 < i64::from(r.y1)
    });
    on_panel && !under_ui
}

/// What libdrm's drmIsMaster does: AUTH_MAGIC is a master-only ioctl, so
/// EACCES means "not master" and any other answer means we are.
fn is_master(card: &Card) -> bool {
    match drm_ffi::auth::auth_magic_token(card.as_fd(), 0) {
        Ok(_) => true,
        Err(e) => e.kind() != io::ErrorKind::PermissionDenied,
    }
}

fn bytes_per_pixel(format: DrmFourcc) -> u32 {
    match format {
        DrmFourcc::Rgb565 => 2,
        _ => 4,
    }
}

impl Display {
    /// Open `card` (a `/dev/dri/cardN` node), become its master, and pick
    /// the connector, the CRTC and the two planes. `Error::NotMaster` means
    /// the display is not ours; the caller may run headless.
    pub fn open(path: &str) -> Result<Self, Error> {
        // Non-blocking is for the event reads; ioctls ignore it.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
            .map_err(|e| Error::Open(path.to_string(), e))?;
        let card = Card(file);
        // Some drivers report an error although the descriptor is master
        // already, hence the confirmation.
        if card.acquire_master_lock().is_err() && !is_master(&card) {
            return Err(Error::NotMaster(path.to_string()));
        }
        let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
        // The tiled plane only routes through the display front end via the
        // atomic API (sun4i decides in its atomic check); the legacy API
        // lands it on the backend.
        card.set_client_capability(ClientCapability::Atomic, true)
            .map_err(|_| Error::NoAtomic(path.to_string()))?;

        let res = card
            .resource_handles()
            .map_err(|e| Error::Io("GETRESOURCES", e))?;
        let (connector, info) = res
            .connectors()
            .iter()
            .filter_map(|&c| card.get_connector(c, true).ok().map(|i| (c, i)))
            .find(|(_, i)| i.state() == connector::State::Connected && !i.modes().is_empty())
            .ok_or(Error::NoConnector)?;
        let first_mode = *info.modes().first().ok_or(Error::NoConnector)?;
        let crtc = info
            .current_encoder()
            .and_then(|e| card.get_encoder(e).ok())
            .and_then(|e| e.crtc())
            .or_else(|| {
                info.encoders()
                    .iter()
                    .filter_map(|&e| card.get_encoder(e).ok())
                    .flat_map(|e| res.filter_crtcs(e.possible_crtcs()))
                    .next()
            })
            .or_else(|| res.crtcs().first().copied())
            .ok_or(Error::NoCrtc)?;
        // The mode the CRTC already runs (the boot splash set it) rather
        // than the connector's first, when it has one.
        let mode = card
            .get_crtc(crtc)
            .ok()
            .and_then(|c| c.mode())
            .filter(|m| m.size().0 != 0)
            .unwrap_or(first_mode);
        let (width, height) = (u32::from(mode.size().0), u32::from(mode.size().1));
        let (width_mm, height_mm) = info.size().unwrap_or((0, 0));

        // Video plane: the primary that takes NV12 (routed through the
        // front end), else any plane that does. UI plane: the first overlay
        // with a usable RGB format, RGB565 first.
        let mut video = None;
        let mut video_fallback = None;
        let mut video_rgb565 = false;
        let mut ui = None;
        for p in card
            .plane_handles()
            .map_err(|e| Error::Io("GETPLANERESOURCES", e))?
        {
            let Ok(pi) = card.get_plane(p) else { continue };
            if !res.filter_crtcs(pi.possible_crtcs()).contains(&crtc) {
                continue;
            }
            let has = |f: DrmFourcc| pi.formats().contains(&(f as u32));
            let kind = plane_type(&card, p);
            if video.is_none() && has(DrmFourcc::Nv12) {
                if video_fallback.is_none() {
                    video_fallback = Some(p);
                }
                if kind == Some(PlaneType::Primary) {
                    video = Some(p);
                    video_rgb565 = has(DrmFourcc::Rgb565);
                }
            }
            if ui.is_none() && kind == Some(PlaneType::Overlay) {
                let preferred = [DrmFourcc::Rgb565, DrmFourcc::Argb8888, DrmFourcc::Xrgb8888];
                if let Some(f) = preferred.into_iter().find(|&f| has(f)) {
                    ui = Some((p, f));
                }
            }
        }
        let video_plane = match video {
            Some(p) => p,
            None => {
                let p = video_fallback.ok_or(Error::NoVideoPlane)?;
                video_rgb565 = card
                    .get_plane(p)
                    .is_ok_and(|pi| pi.formats().contains(&(DrmFourcc::Rgb565 as u32)));
                p
            }
        };
        let ui = ui.filter(|(p, _)| *p != video_plane);
        // The sprite plane: another overlay that takes ARGB8888.
        let mut sprite = None;
        for p in card
            .plane_handles()
            .map_err(|e| Error::Io("GETPLANERESOURCES", e))?
        {
            let Ok(pi) = card.get_plane(p) else { continue };
            if p == video_plane
                || ui.is_some_and(|(u, _)| u == p)
                || !res.filter_crtcs(pi.possible_crtcs()).contains(&crtc)
                || plane_type(&card, p) != Some(PlaneType::Overlay)
                || !pi.formats().contains(&(DrmFourcc::Argb8888 as u32))
            {
                continue;
            }
            if let Ok(props) = plane_props(&card, p) {
                sprite = Some((u32::from(p), props));
                break;
            }
        }

        // The cursor plane: one that takes ARGB8888 on our CRTC.
        let mut cursor = None;
        if std::env::var("LEHU_MAP_CURSOR").is_ok_and(|v| v == "0") {
            log::info!("{path}: LEHU_MAP_CURSOR=0: the cursor plane is left alone");
        } else {
            for p in card
                .plane_handles()
                .map_err(|e| Error::Io("GETPLANERESOURCES", e))?
            {
                let Ok(pi) = card.get_plane(p) else { continue };
                if !res.filter_crtcs(pi.possible_crtcs()).contains(&crtc)
                    || plane_type(&card, p) != Some(PlaneType::Cursor)
                    || !pi.formats().contains(&(DrmFourcc::Argb8888 as u32))
                {
                    continue;
                }
                if let Ok(props) = plane_props(&card, p) {
                    cursor = Some((u32::from(p), props));
                    break;
                }
            }
        }
        // As the driver says, both ways; 64 x 64 squares only where it is.
        let cap = |c| card.get_driver_capability(c).unwrap_or(0);
        let cursor_max =
            if cap(DriverCapability::CursorWidth).min(cap(DriverCapability::CursorHeight)) >= 64 {
                64
            } else {
                CURSOR_SIZE
            };

        let video_props = plane_props(&card, video_plane)?;
        let ui_plane = match ui {
            Some((p, format)) => Some(UiPlane {
                id: u32::from(p),
                props: plane_props(&card, p)?,
                format,
                bpp: bytes_per_pixel(format),
            }),
            None => None,
        };
        let crtc_mode = prop(&card, crtc, "MODE_ID")?;
        let crtc_active = prop(&card, crtc, "ACTIVE")?;
        let conn_crtc = prop(&card, connector, "CRTC_ID")?;
        let mode_blob = match card
            .create_property_blob(&mode)
            .map_err(|e| Error::Io("CREATEPROPBLOB", e))?
        {
            property::Value::Blob(id) => id,
            _ => return Err(Error::Property("MODE_ID blob")),
        };

        let ui_buf = match &ui_plane {
            Some(u) => Some(Dumb::create(&card, width, height, u.format, u.bpp)?),
            None => None,
        };
        let ui_pitch = ui_buf.as_ref().map_or(0, |d| d.pitch);

        let video_id = u32::from(video_plane);
        match &ui_plane {
            Some(u) => log::info!(
                "{path}: crtc {} {width}x{height}, video plane {video_id}, ui plane {} ({})",
                u32::from(crtc),
                u.id,
                u.format
            ),
            None => log::warn!(
                "{path}: crtc {} {width}x{height}, video plane {video_id}, no RGB overlay plane: the UI is disabled",
                u32::from(crtc)
            ),
        }

        Ok(Self {
            card,
            width,
            height,
            width_mm,
            height_mm,
            connector: u32::from(connector),
            crtc: u32::from(crtc),
            mode_blob,
            conn_crtc,
            crtc_mode,
            crtc_active,
            video_plane: video_id,
            video_props,
            video_rgb565,
            ui_plane,
            sprite_plane: sprite,
            cursor_plane: cursor,
            cursor_max,
            ui_pitch,
            video_frames: AtomicU32::new(0),
            shot_dir: std::env::var_os("LEHU_SHOT_DIR").map(std::path::PathBuf::from),
            state: Mutex::new(State {
                request: Request::new(),
                modeset_done: false,
                ui: ui_buf,
                band: Band::default(),
                cache: FbCache::with_capacity(CACHE_CAPACITY),
                video_held: false,
                canvas_full_range: false,
                placeholder: None,
                placeholder_live: 0,
                sprite_on: false,
                sprite_refused: false,
                cursor_want: None,
                cursor_fb: 0,
                cursor_refused: false,
                warned: Warned::default(),
            }),
        })
    }

    /// The first `/dev/dri/card*` node, in name order, that gives a
    /// display. card0 is not always the display: a board can enumerate a
    /// render-only or secondary device first.
    pub fn open_any() -> Result<Self, Error> {
        let dir = std::fs::read_dir("/dev/dri").map_err(|e| Error::Open("/dev/dri".into(), e))?;
        let mut names: Vec<String> = dir
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("card"))
            .collect();
        names.sort();
        for name in &names {
            let path = format!("/dev/dri/{name}");
            match Self::open(&path) {
                Ok(d) => return Ok(d),
                Err(e) => log::warn!("{e}"),
            }
        }
        Err(Error::NoCard)
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The panel's size in millimetres as the connector reports it, (0, 0)
    /// when it reports nothing: unknown, not a zero-sized panel.
    pub fn size_mm(&self) -> (u32, u32) {
        (self.width_mm, self.height_mm)
    }

    /// Whether there is an overlay plane for the UI. Without one every UI
    /// call is a no-op that returns false.
    pub fn has_ui(&self) -> bool {
        self.ui_plane.is_some()
    }

    /// The UI buffer's pixel format, from what the overlay plane supports:
    /// RGB565 preferred, then ARGB8888, then XRGB8888 (ARGB8888 without an
    /// overlay plane).
    pub fn ui_format(&self) -> DrmFourcc {
        self.ui_plane
            .as_ref()
            .map_or(DrmFourcc::Argb8888, |u| u.format)
    }

    /// Bytes per pixel of `ui_format`.
    pub fn ui_bpp(&self) -> u32 {
        self.ui_plane.as_ref().map_or(4, |u| u.bpp)
    }

    /// The UI buffer's row pitch in bytes (0 without a UI). Informational:
    /// `ui_write_rect` takes the caller's pitch.
    pub fn ui_pitch(&self) -> u32 {
        self.ui_pitch
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn commit(&self, st: &mut State, flags: u32) -> io::Result<()> {
        if st.request.overflowed() {
            return Err(io::Error::other("atomic request too large"));
        }
        let (objects, counts, props, values) = st.request.parts();
        drm_ffi::mode::atomic_commit(self.card.as_fd(), flags, objects, counts, props, values)
    }

    /// The connector, CRTC and mode, if they are not set yet; the flags the
    /// commit then needs.
    fn add_modeset(&self, st: &mut State) -> u32 {
        if st.modeset_done {
            return 0;
        }
        let req = &mut st.request;
        req.add(self.connector, self.conn_crtc, u64::from(self.crtc));
        req.add(self.crtc, self.crtc_mode, self.mode_blob);
        req.add(self.crtc, self.crtc_active, 1);
        ALLOW_MODESET
    }

    /// `plane` over the whole panel, showing `src_w` x `src_h` of `fb`.
    fn add_plane_full(
        &self,
        req: &mut Request,
        plane: u32,
        p: &PlaneProps,
        fb: u32,
        src_w: u32,
        src_h: u32,
    ) {
        req.add(plane, p.fb, u64::from(fb));
        req.add(plane, p.crtc, u64::from(self.crtc));
        req.add(plane, p.src_x, 0);
        req.add(plane, p.src_y, 0);
        req.add(plane, p.src_w, u64::from(src_w) << 16);
        req.add(plane, p.src_h, u64::from(src_h) << 16);
        req.add(plane, p.crtc_x, 0);
        req.add(plane, p.crtc_y, 0);
        req.add(plane, p.crtc_w, u64::from(self.width));
        req.add(plane, p.crtc_h, u64::from(self.height));
        if let Some((id, limited, _)) = p.color_range {
            req.add(plane, id, limited);
        }
    }

    /// The UI plane over `r` only, the same pixels of the buffer as of the
    /// panel: a window above the video instead of a plane over the whole
    /// screen (with no per-pixel alpha in RGB565, a full-size plane would
    /// black the video out).
    fn add_ui_window(&self, req: &mut Request, ui: &UiPlane, fb: u32, r: Rect) {
        let p = &ui.props;
        req.add(ui.id, p.fb, u64::from(fb));
        req.add(ui.id, p.crtc, u64::from(self.crtc));
        req.add(ui.id, p.src_x, u64::from(r.x0) << 16);
        req.add(ui.id, p.src_y, u64::from(r.y0) << 16);
        req.add(ui.id, p.src_w, u64::from(r.width()) << 16);
        req.add(ui.id, p.src_h, u64::from(r.height()) << 16);
        req.add(ui.id, p.crtc_x, u64::from(r.x0));
        req.add(ui.id, p.crtc_y, u64::from(r.y0));
        req.add(ui.id, p.crtc_w, u64::from(r.width()));
        req.add(ui.id, p.crtc_h, u64::from(r.height()));
        // Above the video, and above the sprite between them.
        if let Some(z) = p.zpos {
            req.add(ui.id, z, 2);
        }
    }

    /// Copy `h` rows of `w` pixels in `ui_format` from `pixels`, whose
    /// rows are `pitch` bytes apart, to (`x`, `y`) of the UI buffer. The
    /// screens' flush lands here: one copy per row into write-combined
    /// memory, nothing read back. False when the rectangle is not inside
    /// the panel or `pixels` does not hold the rows.
    pub fn ui_write_rect(
        &self,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        pixels: &[u8],
        pitch: usize,
    ) -> bool {
        let Some(ui) = &self.ui_plane else {
            return false;
        };
        if !band::inside(x, y, w, h, self.width, self.height) {
            return false;
        }
        // x and w are below the panel's width, a u16: no overflow here.
        let bpp = ui.bpp as usize;
        let st = self.lock();
        let Some(buf) = &st.ui else {
            return false;
        };
        buf.write_rows(
            x as usize * bpp,
            y as usize,
            h as usize,
            w as usize * bpp,
            pixels,
            pitch,
        )
    }

    /// Show the UI plane over rows `y0..y1` at full width: the whole screen
    /// for a home screen, a band for a toast, so rows without content never
    /// occlude the video below. A no-op when the plane already shows that
    /// band. Waits for the flip; the first call is the modeset.
    pub fn ui_show(&self, y0: i32, y1: i32) -> bool {
        let Some(r) = band::clip(
            0,
            i64::from(y0),
            i64::from(self.width),
            i64::from(y1),
            self.width,
            self.height,
        ) else {
            return false;
        };
        let mut st = self.lock();
        self.ui_show_locked(&mut st, r, true)
    }

    /// The UI plane over just `w` x `h` at (`x`, `y`): a window onto the UI
    /// buffer above live video (the volume bar, the camera's column),
    /// composited by the display engine. With `wait` false it never blocks
    /// on a vblank: false then means a flip is still pending, try again on
    /// a later turn, so the video path never waits behind the UI.
    pub fn ui_show_window(&self, x: i32, y: i32, w: u32, h: u32, wait: bool) -> bool {
        let (x, y) = (i64::from(x), i64::from(y));
        let Some(r) = band::clip(
            x,
            y,
            x + i64::from(w),
            y + i64::from(h),
            self.width,
            self.height,
        ) else {
            return false;
        };
        let mut st = self.lock();
        self.ui_show_locked(&mut st, r, wait)
    }

    fn ui_show_locked(&self, st: &mut State, r: Rect, wait: bool) -> bool {
        let Some(ui) = &self.ui_plane else {
            return false;
        };
        let Some(fb) = st.ui.as_ref().map(|d| u32::from(d.fb)) else {
            return false;
        };
        if st.band.same(r) {
            return true;
        }
        st.request.clear();
        let flags = self.add_modeset(st);
        self.add_ui_window(&mut st.request, ui, fb, r);
        let mut cursor_fb = self.add_cursor(st, Some(r));

        // Once the mode is up, commit without waiting for the flip: a
        // blocking commit parks the caller for a vblank (about 16 ms) on
        // every UI update. A failure (EBUSY: the previous flip is pending)
        // then waits, unless the caller would rather come back -- it holds
        // the lock the video path needs to present.
        let mut shown = false;
        let mut err = None;
        if st.modeset_done {
            shown = self.commit(st, flags | NONBLOCK).is_ok();
            if !shown && !wait {
                return false;
            }
        }
        if !shown {
            match self.commit(st, flags) {
                Ok(()) => shown = true,
                Err(e) => err = Some(e),
            }
        }
        if !shown && !st.modeset_done {
            // The UI-only modeset was refused: enable the video plane too,
            // with the black placeholder. The first real frame replaces it.
            if st.placeholder.is_none() {
                let (format, bpp) = if self.video_rgb565 {
                    (DrmFourcc::Rgb565, 2)
                } else {
                    (DrmFourcc::Xrgb8888, 4)
                };
                match Dumb::create(&self.card, self.width, self.height, format, bpp) {
                    Ok(d) => st.placeholder = Some(d),
                    Err(e) => log::warn!("placeholder framebuffer: {e}"),
                }
            }
            if let Some(pfb) = st.placeholder.as_ref().map(|d| u32::from(d.fb)) {
                st.request.clear();
                let flags = self.add_modeset(st);
                self.add_ui_window(&mut st.request, ui, fb, r);
                cursor_fb = self.add_cursor(st, Some(r));
                self.add_plane_full(
                    &mut st.request,
                    self.video_plane,
                    &self.video_props,
                    pfb,
                    self.width,
                    self.height,
                );
                match self.commit(st, flags) {
                    Ok(()) => shown = true,
                    Err(e) => err = Some(e),
                }
                st.placeholder_live = 0;
            }
        }
        if !shown {
            if !st.warned.ui_commit {
                st.warned.ui_commit = true;
                match err {
                    Some(e) => log::warn!("ui commit failed: {e}"),
                    None => log::warn!("ui commit failed"),
                }
            }
            return false;
        }
        st.modeset_done = true;
        st.band.set(r);
        st.cursor_fb = cursor_fb;
        true
    }

    /// Disable the UI plane. `wait` as for `ui_show_window`: false then
    /// means a flip is still pending and the plane is still up.
    pub fn ui_hide(&self, wait: bool) -> bool {
        let Some(ui) = &self.ui_plane else {
            return false;
        };
        let mut st = self.lock();
        if st.ui.is_none() || !st.band.is_visible() || !st.modeset_done {
            return true;
        }
        st.request.clear();
        st.request.add(ui.id, ui.props.fb, 0);
        st.request.add(ui.id, ui.props.crtc, 0);
        let cursor_fb = self.add_cursor(&mut st, None);
        let flags = if wait { 0 } else { NONBLOCK };
        if self.commit(&mut st, flags).is_err() {
            return false;
        }
        st.band.clear();
        st.cursor_fb = cursor_fb;
        true
    }

    /// Put the phone's picture on the video plane, scaled to the panel.
    /// Called from the decoder thread; serialised against the UI's commits.
    /// Does the modeset if the UI has not.
    ///
    /// Never waits for a pending flip once the mode is set: `Busy` hands the
    /// frame back, because a decoder parked on the vblank cannot keep up
    /// with a 60 fps stream, and the frames it falls behind on get dropped
    /// from its queue, references included, which corrupts every picture
    /// after the gap. Decode every frame, present only the newest.
    ///
    /// After `Shown` the buffer may stay on scanout until the next `Shown`,
    /// and longer: a non-blocking commit leaves frame N scanning while N+1
    /// is queued, and the sun4i CRTC signals a flip one frame early when a
    /// commit starts before the previous one has latched, so during
    /// continuous motion the frame two commits back can still be on screen.
    /// A buffer returned to the decoder then is written mid-scan. The owner
    /// keeps the last three shown frames referenced.
    pub fn show_video(&self, frame: &Frame) -> Show {
        let mut st = self.lock();
        if st.video_held {
            return Show::Busy;
        }
        self.show_locked(&mut st, frame, false)
    }

    /// A camera on the video plane: the same import and flip, not counted
    /// in `video_frames`, and shown while `hold_video` keeps the decoder's
    /// frames off the plane. The caller keeps each buffer alive as
    /// `show_video` describes, and calls `flush_video` before freeing them.
    pub fn show_camera(&self, frame: &Frame) -> Show {
        let mut st = self.lock();
        self.show_locked(&mut st, frame, true)
    }

    /// The video plane can be told its YCbCr range (`COLOR_RANGE`): with
    /// it, a map canvas may be left full range as the JPEG engine decodes
    /// it, and the display converts accordingly.
    pub fn has_color_range(&self) -> bool {
        self.video_props.color_range.is_some()
    }

    /// Map canvases shown from now are full-range YCbCr (true) or limited
    /// (false); nothing happens without `has_color_range`.
    pub fn set_canvas_full_range(&self, full: bool) {
        self.lock().canvas_full_range = full;
    }

    /// While held, `show_video` returns `Busy` without touching the plane.
    pub fn hold_video(&self, hold: bool) {
        self.lock().video_held = hold;
    }

    /// Frames of the phone's video presented so far, for "is video flowing"
    /// checks. Lock-free: safe to poll while a commit is in flight.
    pub fn video_frames(&self) -> u32 {
        self.video_frames.load(Ordering::Relaxed)
    }

    /// Drop the cached framebuffers and their GEM handles. Call it from the
    /// decoder's (and the camera's) teardown, before its buffers are freed:
    /// the display stays open, so without this the handles would keep
    /// references on the freed pool, a CMA leak per rebuild, and this
    /// board's CMA fits one pool.
    pub fn flush_video(&self) {
        let mut st = self.lock();
        st.cache.flush(&self.card);
    }

    fn show_locked(&self, st: &mut State, frame: &Frame, camera: bool) -> Show {
        if let Err(e) = frame.check() {
            if !st.warned.frame {
                st.warned.frame = true;
                log::warn!("frame refused: {e}");
            }
            return Show::Failed;
        }
        let Some(first) = frame.planes().first() else {
            return Show::Failed;
        };
        // The framebuffer for this pool buffer, built once; only on a miss
        // are the dma-bufs imported.
        let fb = match st.cache.lookup(
            first.fd,
            frame.width,
            frame.height,
            frame.fourcc,
            &self.card,
        ) {
            Some(fb) => fb,
            None => match self.import(frame) {
                Ok(entry) => {
                    let fb = entry.fb;
                    st.cache.insert(entry, &self.card);
                    fb
                }
                Err(e) => {
                    if !st.warned.import {
                        st.warned.import = true;
                        log::warn!("frame not imported: {e}");
                    }
                    return Show::Failed;
                }
            },
        };

        st.request.clear();
        let flags = self.add_modeset(st);
        if st.sprite_on {
            self.add_sprite_off(&mut st.request);
        }
        st.cursor_want = None;
        let cursor_fb = self.add_cursor(st, None);
        self.add_plane_full(
            &mut st.request,
            self.video_plane,
            &self.video_props,
            fb,
            frame.width,
            frame.height,
        );
        // Non-blocking, so the decode thread never parks on the vblank.
        // Only the first (modeset) commit and failures other than EBUSY
        // take the blocking path.
        let mut shown = false;
        let mut err = None;
        if st.modeset_done {
            match self.commit(st, flags | NONBLOCK) {
                Ok(()) => shown = true,
                Err(e) if e.kind() == io::ErrorKind::ResourceBusy => return Show::Busy,
                Err(_) => {}
            }
        }
        if !shown {
            match self.commit(st, flags) {
                Ok(()) => shown = true,
                Err(e) => err = Some(e),
            }
        }
        if !shown {
            if !st.warned.video_commit {
                st.warned.video_commit = true;
                match err {
                    Some(e) => log::warn!("video commit failed: {e}"),
                    None => log::warn!("video commit failed"),
                }
            }
            // The framebuffer stays cached: a passing failure does not
            // throw away the import.
            return Show::Failed;
        }
        st.modeset_done = true;
        st.sprite_on = false;
        st.cursor_fb = cursor_fb;
        // Only the phone's frames say "video is flowing".
        if !camera {
            self.video_frames.fetch_add(1, Ordering::Relaxed);
        }
        // Video owns the primary plane now; the placeholder can go. One
        // extra commit, so the flip that replaced it has certainly completed.
        if st.placeholder.is_some() {
            st.placeholder_live += 1;
            if st.placeholder_live >= 2 {
                if let Some(p) = st.placeholder.take() {
                    p.destroy(&self.card);
                }
                st.placeholder_live = 0;
            }
        }
        Show::Shown
    }

    /// Import the frame's dma-bufs and make a framebuffer. The descriptors
    /// belong to their owner and are never closed here; the kernel gives
    /// one GEM handle per dma-buf, so the handles stay bounded by the pool.
    fn import(&self, f: &Frame) -> Result<Entry, Error> {
        let planes = f.planes();
        let first = planes
            .first()
            .ok_or(Error::Frame(crate::frame::FrameError::NoPlanes))?;
        let mut handles: [Option<GemHandle>; MAX_PLANES] = [None; MAX_PLANES];
        let mut raw = [0u32; MAX_PLANES];
        let mut pitches = [0u32; MAX_PLANES];
        let mut offsets = [0u32; MAX_PLANES];
        for (i, p) in planes.iter().enumerate() {
            // SAFETY: `check` saw the descriptor is not negative, and its
            // owner keeps it open while the picture is ours.
            let fd = unsafe { BorrowedFd::borrow_raw(p.fd) };
            match self.card.prime_fd_to_buffer(fd) {
                Ok(h) => {
                    handles[i] = Some(h);
                    raw[i] = u32::from(h);
                    pitches[i] = p.pitch;
                    offsets[i] = p.offset;
                }
                Err(e) => {
                    cache::close_handles(&raw[..i], &self.card);
                    return Err(Error::Io("PRIME_FD_TO_HANDLE", e));
                }
            }
        }
        let format = match DrmFourcc::try_from(f.fourcc) {
            Ok(fmt) => fmt,
            Err(_) => {
                cache::close_handles(&raw, &self.card);
                return Err(Error::Frame(crate::frame::FrameError::Fourcc(f.fourcc)));
            }
        };
        let planar = Planar {
            size: (f.width, f.height),
            format,
            modifier: Some(DrmModifier::from(f.modifier)),
            pitches,
            handles,
            offsets,
        };
        match self
            .card
            .add_planar_framebuffer(&planar, FbCmd2Flags::MODIFIERS)
        {
            Ok(fb) => Ok(Entry {
                key: first.fd,
                fb: u32::from(fb),
                handles: raw,
                handle_count: planes.len(),
                width: f.width,
                height: f.height,
                fourcc: f.fourcc,
            }),
            Err(e) => {
                cache::close_handles(&raw, &self.card);
                Err(Error::Io("ADDFB2", e))
            }
        }
    }

    /// The card's descriptor for the main loop's poll: readable when a flip
    /// completed. Every non-blocking commit asks for a flip event, so
    /// `handle_events` must run whenever this is readable: events left
    /// unread fill the kernel's small per-file queue, and commits then fail.
    pub fn fd(&self) -> BorrowedFd<'_> {
        self.card.as_fd()
    }

    /// Read the flip completions queued on the descriptor; how many there
    /// were. Never blocks. After a `Busy` or a false from a non-waiting UI
    /// call, a completed flip is the moment to try again.
    pub fn handle_events(&self) -> u32 {
        match self.card.receive_events() {
            Ok(events) => events.filter(|e| matches!(e, Event::PageFlip(_))).count() as u32,
            Err(_) => 0,
        }
    }
}

/// A picture the program draws itself (the map) for the video plane: a
/// dumb buffer, mapped, larger than the panel, of which the plane shows a
/// panel-sized window. Moving the window is a commit, no copy. Owned by
/// whoever draws into it, on any thread: the mapping is its alone.
/// Write-combined memory: written in rows, never read back.
pub struct Canvas {
    dumb: Dumb,
    width: u32,
    height: u32,
    format: CanvasFormat,
    /// For the cursor plane: ARGB8888 with premultiplied alpha, as DRM
    /// planes take it unless told otherwise.
    cursor: bool,
}

/// A canvas's pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasFormat {
    /// RGB565, two bytes a pixel, little-endian.
    Rgb565,
    /// XRGB8888, four bytes a pixel, little-endian (B, G, R, X).
    Xrgb8888,
    /// NV12, limited range: `height` rows of luma, then `height / 2` rows
    /// of Cb, Cr pairs, all `pitch` apart. The window may sit at
    /// any pixel: at an odd one the chroma (one pair for two by two
    /// pixels) is half a pixel off, which nobody sees, where steps of two
    /// show as a judder when the map glides.
    Nv12,
    /// ARGB8888, four bytes a pixel, little-endian (B, G, R, A), alpha
    /// not premultiplied: a sprite's.
    Argb8888,
}

/// A part of a sprite canvas on the sprite plane: the `w` x `h` pixels
/// at (`sx`, `sy`) of `canvas`, at (`x`, `y`) of the panel. The parts
/// off the panel are cut off.
#[derive(Clone, Copy)]
pub struct Sprite<'a> {
    pub canvas: &'a Canvas,
    pub sx: u32,
    pub sy: u32,
    pub w: u32,
    pub h: u32,
    pub x: i32,
    pub y: i32,
}

impl Canvas {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> CanvasFormat {
        self.format
    }

    /// Bytes from one row to the next (of either plane, in NV12).
    pub fn pitch(&self) -> usize {
        self.dumb.pitch as usize
    }

    /// Where the chroma rows begin, in NV12; the buffer's end otherwise.
    pub fn chroma_offset(&self) -> usize {
        match self.format {
            CanvasFormat::Nv12 => self.pitch() * self.height as usize,
            _ => self.dumb.size,
        }
    }

    /// The whole buffer, rows `pitch` apart.
    pub fn pixels(&mut self) -> &mut [u8] {
        // SAFETY: the mapping made in `Dumb::create`, `size` bytes, alive as
        // long as the canvas; `&mut self` makes this the only view of it.
        unsafe { std::slice::from_raw_parts_mut(self.dumb.map.as_ptr(), self.dumb.size) }
    }
}

impl Display {
    /// A canvas of `width` x `height` for the video plane, black: NV12 when
    /// asked for (the video plane always takes it; `width` and `height`
    /// even), else in the plane's RGB format (RGB565 when it takes it,
    /// else XRGB8888).
    pub fn canvas(&self, width: u32, height: u32, nv12: bool) -> Result<Canvas, Error> {
        let (format, fourcc, bpp) = if nv12 {
            if !width.is_multiple_of(2) || !height.is_multiple_of(2) {
                return Err(Error::Io(
                    "canvas",
                    io::Error::other("NV12 wants an even size"),
                ));
            }
            (CanvasFormat::Nv12, DrmFourcc::Nv12, 1)
        } else if self.video_rgb565 {
            (CanvasFormat::Rgb565, DrmFourcc::Rgb565, 2)
        } else {
            (CanvasFormat::Xrgb8888, DrmFourcc::Xrgb8888, 4)
        };
        let dumb = Dumb::create(&self.card, width, height, fourcc, bpp)?;
        Ok(Canvas {
            dumb,
            width,
            height,
            format,
            cursor: false,
        })
    }

    /// There is a plane with per-pixel alpha above the video and beneath
    /// the UI, and the driver has not refused it.
    pub fn has_sprite(&self) -> bool {
        self.sprite_plane.is_some() && !self.lock().sprite_refused
    }

    /// A transparent ARGB8888 canvas of `width` x `height` for the sprite
    /// plane.
    pub fn sprite_canvas(&self, width: u32, height: u32) -> Result<Canvas, Error> {
        let dumb = Dumb::create(&self.card, width, height, DrmFourcc::Argb8888, 4)?;
        Ok(Canvas {
            dumb,
            width,
            height,
            format: CanvasFormat::Argb8888,
            cursor: false,
        })
    }

    /// There is a cursor plane (refused or not): without taking the lock.
    pub fn has_cursor_plane(&self) -> bool {
        self.cursor_plane.is_some()
    }

    /// There is a cursor plane, and the driver has not refused it.
    pub fn has_cursor(&self) -> bool {
        self.cursor_plane.is_some() && !self.lock().cursor_refused
    }

    /// The largest square the cursor plane takes, `CURSOR_SIZE` or 64.
    pub fn cursor_max(&self) -> u32 {
        self.cursor_max
    }

    /// A transparent square for the cursor plane, `size` a side (from
    /// `CURSOR_SIZE` to `cursor_max`): ARGB8888, alpha premultiplied. A
    /// `Sprite` of it is its whole, at any place, partly off the panel too.
    pub fn cursor_canvas(&self, size: u32) -> Result<Canvas, Error> {
        let size = size.clamp(CURSOR_SIZE, self.cursor_max);
        let dumb = Dumb::create(&self.card, size, size, DrmFourcc::Argb8888, 4)?;
        Ok(Canvas {
            dumb,
            width: size,
            height: size,
            format: CanvasFormat::Argb8888,
            cursor: true,
        })
    }

    /// The cursor plane holds `canvas` now: drawing into it would not
    /// show, the driver having taken its pixels already.
    pub fn cursor_holds(&self, canvas: &Canvas) -> bool {
        self.lock().cursor_fb == u32::from(canvas.dumb.fb)
    }

    /// The cursor's place for a sprite of a cursor canvas.
    fn cursor_at(s: &Sprite<'_>) -> CursorAt {
        CursorAt {
            fb: u32::from(s.canvas.dumb.fb),
            size: s.canvas.width,
            x: s.x,
            y: s.y,
        }
    }

    /// The cursor plane as `want` and the UI window `ui` have it, in the
    /// request; the framebuffer it then holds (0: off). Nothing is added
    /// when it stays off.
    fn add_cursor(&self, st: &mut State, ui: Option<Rect>) -> u32 {
        let Some((id, p)) = &self.cursor_plane else {
            return 0;
        };
        let at = st
            .cursor_want
            .filter(|a| cursor_visible(*a, self.width, self.height, ui));
        let req = &mut st.request;
        match at {
            Some(a) => {
                let size = u64::from(a.size);
                req.add(*id, p.fb, u64::from(a.fb));
                req.add(*id, p.crtc, u64::from(self.crtc));
                req.add(*id, p.src_x, 0);
                req.add(*id, p.src_y, 0);
                req.add(*id, p.src_w, size << 16);
                req.add(*id, p.src_h, size << 16);
                // CRTC_X and CRTC_Y are signed: off the left or top edge
                // is a negative number, as the property's 64 bits.
                req.add(*id, p.crtc_x, i64::from(a.x) as u64);
                req.add(*id, p.crtc_y, i64::from(a.y) as u64);
                req.add(*id, p.crtc_w, size);
                req.add(*id, p.crtc_h, size);
                a.fb
            }
            None => {
                if st.cursor_fb != 0 {
                    req.add(*id, p.fb, 0);
                    req.add(*id, p.crtc, 0);
                }
                0
            }
        }
    }

    /// The sprite plane off, in the request.
    fn add_sprite_off(&self, req: &mut Request) {
        if let Some((id, p)) = &self.sprite_plane {
            req.add(*id, p.fb, 0);
            req.add(*id, p.crtc, 0);
        }
    }

    /// The sprite plane showing `s`, in the request; false when none of
    /// it is on the panel.
    fn add_sprite(&self, req: &mut Request, s: &Sprite<'_>) -> bool {
        let Some((id, p)) = &self.sprite_plane else {
            return false;
        };
        // Cut to the panel: the plane is never placed partly off it.
        let (pw, ph) = (i64::from(self.width), i64::from(self.height));
        let (x0, y0) = (i64::from(s.x).max(0), i64::from(s.y).max(0));
        let x1 = (i64::from(s.x) + i64::from(s.w)).min(pw);
        let y1 = (i64::from(s.y) + i64::from(s.h)).min(ph);
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let sx = i64::from(s.sx) + x0 - i64::from(s.x);
        let sy = i64::from(s.sy) + y0 - i64::from(s.y);
        let (w, h) = ((x1 - x0) as u64, (y1 - y0) as u64);
        req.add(*id, p.fb, u64::from(u32::from(s.canvas.dumb.fb)));
        req.add(*id, p.crtc, u64::from(self.crtc));
        req.add(*id, p.src_x, (sx as u64) << 16);
        req.add(*id, p.src_y, (sy as u64) << 16);
        req.add(*id, p.src_w, w << 16);
        req.add(*id, p.src_h, h << 16);
        req.add(*id, p.crtc_x, x0 as u64);
        req.add(*id, p.crtc_y, y0 as u64);
        req.add(*id, p.crtc_w, w);
        req.add(*id, p.crtc_h, h);
        if let Some(z) = p.zpos {
            req.add(*id, z, 1);
        }
        true
    }

    /// Wait for the panel's refresh number `sequence`, or the next one if
    /// it has gone by (None: the next refresh); the number of the one
    /// waited for. Refreshes are counted by the kernel from when the CRTC
    /// came up; the display's lock is not held meanwhile.
    pub fn wait_vblank(&self, sequence: Option<u32>) -> io::Result<u32> {
        const ABSOLUTE: u32 = 0;
        const RELATIVE: u32 = 1;
        const NEXT_ON_MISS: u32 = 0x1000_0000;
        let (kind, n) = match sequence {
            Some(n) => (ABSOLUTE | NEXT_ON_MISS, n),
            None => (RELATIVE, 1),
        };
        drm_ffi::wait_vblank(self.card.as_fd(), kind, n, 0).map(|r| r.sequence)
    }

    /// As `wait_vblank`, with the refresh's time on the monotonic clock
    /// (as the kernel took it at the refresh, not when this thread woke).
    pub fn wait_vblank_time(&self, sequence: Option<u32>) -> io::Result<(u32, Duration)> {
        const ABSOLUTE: u32 = 0;
        const RELATIVE: u32 = 1;
        const NEXT_ON_MISS: u32 = 0x1000_0000;
        let (kind, n) = match sequence {
            Some(n) => (ABSOLUTE | NEXT_ON_MISS, n),
            None => (RELATIVE, 1),
        };
        let r = drm_ffi::wait_vblank(self.card.as_fd(), kind, n, 0)?;
        let at = Duration::new(
            u64::try_from(r.tval_sec).unwrap_or(0),
            u32::try_from(r.tval_usec).unwrap_or(0).min(999_999) * 1000,
        );
        Ok((r.sequence, at))
    }

    /// Take the sprite down, waiting for the flip.
    pub fn hide_sprite(&self) {
        let mut st = self.lock();
        st.cursor_want = None;
        if !st.sprite_on && st.cursor_fb == 0 {
            return;
        }
        st.request.clear();
        if st.sprite_on {
            self.add_sprite_off(&mut st.request);
        }
        let cursor_fb = self.add_cursor(&mut st, None);
        if self.commit(&mut st, 0).is_ok() {
            st.sprite_on = false;
            st.cursor_fb = cursor_fb;
        }
    }

    /// Show the panel-sized window of `canvas` whose top left is (`x`,
    /// `y`), 1:1, on the video plane. Like `show_camera`: the decoder's
    /// frames wait while `hold_video` holds them, a pending flip gives
    /// `Busy`, and the canvas stays on scanout until a later picture
    /// replaces it (keep the last three shown alive). The sprite plane
    /// goes off.
    pub fn show_canvas(&self, canvas: &Canvas, x: u32, y: u32) -> Show {
        self.show_canvas_with(canvas, x, y, None)
    }

    /// `show_canvas`, and the sprite plane in the same commit: showing
    /// `sprite`, or off. A sprite canvas shown stays on scanout as a
    /// canvas does. Should the driver refuse the sprite plane, the canvas
    /// is shown without it, and `has_sprite` is false from then on.
    pub fn show_canvas_with(
        &self,
        canvas: &Canvas,
        x: u32,
        y: u32,
        sprite: Option<Sprite<'_>>,
    ) -> Show {
        let src = [x << 16, y << 16, self.width << 16, self.height << 16];
        self.show_canvas_src(canvas, src, sprite)
    }

    /// As `show_canvas_with`, the part of `canvas` at `src` (x, y, width,
    /// height, in 1/65536 pixels) scaled to the panel by the display's
    /// scaler: a picture enlarged or shrunk smoothly, as when the map
    /// changes zoom.
    pub fn show_canvas_scaled(
        &self,
        canvas: &Canvas,
        src: [u32; 4],
        sprite: Option<Sprite<'_>>,
    ) -> Show {
        self.show_canvas_src(canvas, src, sprite)
    }

    fn show_canvas_src(&self, canvas: &Canvas, src: [u32; 4], sprite: Option<Sprite<'_>>) -> Show {
        let shown = self.show_canvas_once(canvas, src, sprite.as_ref());
        if shown != Show::Failed || sprite.is_none() {
            return shown;
        }
        // Without it, then: if that is shown, the sprite (or the cursor)
        // was the trouble.
        let again = self.show_canvas_once(canvas, src, None);
        if again == Show::Shown {
            if sprite.is_some_and(|s| s.canvas.cursor) {
                self.lock().cursor_refused = true;
                log::warn!("the cursor plane was refused; the map draws its marker itself");
            } else {
                self.lock().sprite_refused = true;
                log::warn!("the sprite plane was refused; the map draws its marker itself");
            }
        }
        again
    }

    fn show_canvas_once(
        &self,
        canvas: &Canvas,
        src: [u32; 4],
        sprite: Option<&Sprite<'_>>,
    ) -> Show {
        let [x, y, w, h] = src.map(u64::from);
        if w == 0
            || h == 0
            || x + w > u64::from(canvas.width) << 16
            || y + h > u64::from(canvas.height) << 16
        {
            return Show::Failed;
        }
        let mut st = self.lock();
        st.request.clear();
        let flags = self.add_modeset(&mut st);
        let p = &self.video_props;
        let plane = self.video_plane;
        let full_range = st.canvas_full_range;
        let req = &mut st.request;
        req.add(plane, p.fb, u64::from(u32::from(canvas.dumb.fb)));
        req.add(plane, p.crtc, u64::from(self.crtc));
        req.add(plane, p.src_x, x);
        req.add(plane, p.src_y, y);
        req.add(plane, p.src_w, w);
        req.add(plane, p.src_h, h);
        req.add(plane, p.crtc_x, 0);
        req.add(plane, p.crtc_y, 0);
        req.add(plane, p.crtc_w, u64::from(self.width));
        req.add(plane, p.crtc_h, u64::from(self.height));
        if let Some((id, limited, full)) = p.color_range {
            req.add(plane, id, if full_range { full } else { limited });
        }
        let on_cursor = sprite.is_some_and(|s| s.canvas.cursor);
        let shot_sprite = sprite;
        st.cursor_want = sprite
            .filter(|_| on_cursor && !st.cursor_refused)
            .map(Self::cursor_at);
        let ui = st.band.shown();
        let cursor_fb = self.add_cursor(&mut st, ui);
        let sprite = sprite.filter(|_| !on_cursor && !st.sprite_refused);
        let sprite_on = match sprite {
            Some(s) => self.add_sprite(&mut st.request, s),
            None => false,
        };
        if !sprite_on && st.sprite_on {
            self.add_sprite_off(&mut st.request);
        }
        if st.modeset_done {
            return match self.commit(&mut st, flags | NONBLOCK) {
                Ok(()) => {
                    st.sprite_on = sprite_on;
                    st.cursor_fb = cursor_fb;
                    if let Some(dir) = &self.shot_dir
                        && std::fs::remove_file(dir.join("take")).is_ok()
                        && let Err(e) = self.write_shot(dir, &st, canvas, src, shot_sprite)
                    {
                        log::warn!("shot: {e}");
                    }
                    Show::Shown
                }
                Err(e) if e.kind() == io::ErrorKind::ResourceBusy => Show::Busy,
                Err(e) => {
                    if !st.warned.video_commit {
                        st.warned.video_commit = true;
                        log::warn!("canvas commit failed: {e}");
                    }
                    Show::Failed
                }
            };
        }
        match self.commit(&mut st, flags) {
            Ok(()) => {
                st.modeset_done = true;
                st.sprite_on = sprite_on;
                st.cursor_fb = cursor_fb;
                Show::Shown
            }
            Err(e) => {
                log::warn!("canvas commit failed: {e}");
                Show::Failed
            }
        }
    }

    /// What the panel shows, to `dir`: `shot.txt` with the sizes and
    /// places, and the bytes of the canvas (`shot-canvas.bin`), the sprite
    /// shown (`shot-sprite.bin`) and the UI buffer (`shot-ui.bin`).
    fn write_shot(
        &self,
        dir: &std::path::Path,
        st: &State,
        canvas: &Canvas,
        src: [u32; 4],
        sprite: Option<&Sprite<'_>>,
    ) -> io::Result<()> {
        use std::io::Write;
        let bytes = |d: &Dumb| -> &[u8] {
            // SAFETY: the mapping made in `Dumb::create`, `size` bytes,
            // alive while the buffer is; read under the display's lock.
            unsafe { std::slice::from_raw_parts(d.map.as_ptr(), d.size) }
        };
        let mut head = String::new();
        head += &format!("panel {} {}\n", self.width, self.height);
        head += &format!(
            "canvas {} {} {} {} {:?} {}\n",
            canvas.width,
            canvas.height,
            canvas.dumb.pitch,
            canvas.chroma_offset(),
            canvas.format,
            st.canvas_full_range
        );
        head += &format!("src {} {} {} {}\n", src[0], src[1], src[2], src[3]);
        std::fs::write(dir.join("shot-canvas.bin"), bytes(&canvas.dumb))?;
        let shown = |s: &&Sprite<'_>| {
            if s.canvas.cursor {
                st.cursor_fb != 0
            } else {
                st.sprite_on
            }
        };
        if let Some(s) = sprite.filter(shown) {
            // The cursor's pixels are premultiplied; it is hidden where
            // the UI would cover it, so the order of the two is moot.
            head += &format!(
                "sprite {} {} {} {} {} {} {} {} {} {}\n",
                s.canvas.width,
                s.canvas.height,
                s.canvas.dumb.pitch,
                s.sx,
                s.sy,
                s.w,
                s.h,
                s.x,
                s.y,
                if s.canvas.cursor {
                    "premultiplied"
                } else {
                    "straight"
                }
            );
            std::fs::write(dir.join("shot-sprite.bin"), bytes(&s.canvas.dumb))?;
        }
        if let (Some(ui), Some(r), Some(p)) = (&st.ui, st.band.shown(), &self.ui_plane) {
            head += &format!(
                "ui {} {} {} {} {} {:?} {}\n",
                r.x0, r.y0, r.x1, r.y1, ui.pitch, p.format, p.bpp
            );
            std::fs::write(dir.join("shot-ui.bin"), bytes(ui))?;
        }
        std::fs::File::create(dir.join("shot.txt"))?.write_all(head.as_bytes())?;
        log::info!("shot: written to {}", dir.display());
        Ok(())
    }

    /// Free a canvas. If it is still on the plane the kernel takes it off.
    pub fn free_canvas(&self, canvas: Canvas) {
        canvas.dumb.destroy(&self.card);
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(PoisonError::into_inner);
        if let Some(ui) = st.ui.take() {
            ui.destroy(&self.card);
        }
        if let Some(p) = st.placeholder.take() {
            p.destroy(&self.card);
        }
        st.cache.flush(&self.card);
        let _ = self.card.destroy_property_blob(self.mode_blob);
        // The descriptor closes with `card`, and DRM master with it.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_is_shared_between_threads() {
        fn shared<T: Send + Sync>() {}
        shared::<Display>();
    }

    #[test]
    fn the_cursor_hides_under_the_ui_and_off_the_panel() {
        let at = |x, y| CursorAt {
            fb: 1,
            size: 32,
            x,
            y,
        };
        let bar = Rect {
            x0: 0,
            y0: 396,
            x1: 800,
            y1: 480,
        };
        assert!(cursor_visible(at(384, 314), 800, 480, Some(bar)));
        assert!(cursor_visible(at(384, 314), 800, 480, None));
        // Touching the bar's top row: under it.
        assert!(!cursor_visible(at(384, 365), 800, 480, Some(bar)));
        // One row above it: clear.
        assert!(cursor_visible(at(384, 364), 800, 480, Some(bar)));
        // Partly off the left and top: shown; wholly off: not.
        assert!(cursor_visible(at(-31, -31), 800, 480, None));
        assert!(!cursor_visible(at(-32, 10), 800, 480, None));
        assert!(!cursor_visible(at(800, 10), 800, 480, None));
        assert!(!cursor_visible(at(10, 480), 800, 480, None));
        // A 64 pixel cursor: by its own size.
        let big = |x, y| CursorAt {
            size: 64,
            ..at(x, y)
        };
        assert!(!cursor_visible(big(384, 333), 800, 480, Some(bar)));
        assert!(cursor_visible(big(384, 332), 800, 480, Some(bar)));
        assert!(cursor_visible(big(-63, -63), 800, 480, None));
        assert!(!cursor_visible(big(-64, 10), 800, 480, None));
    }

    #[test]
    fn pixel_sizes_follow_the_format() {
        assert_eq!(bytes_per_pixel(DrmFourcc::Rgb565), 2);
        assert_eq!(bytes_per_pixel(DrmFourcc::Argb8888), 4);
        assert_eq!(bytes_per_pixel(DrmFourcc::Xrgb8888), 4);
    }

    #[test]
    fn non_blocking_commits_ask_for_an_event() {
        assert_ne!(NONBLOCK & drm_ffi::DRM_MODE_PAGE_FLIP_EVENT, 0);
        assert_ne!(NONBLOCK & drm_ffi::DRM_MODE_ATOMIC_NONBLOCK, 0);
        assert_eq!(NONBLOCK & ALLOW_MODESET, 0);
    }
}
