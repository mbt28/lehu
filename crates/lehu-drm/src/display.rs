//! The device: opening the card, choosing the planes, and every commit.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use drm::buffer::{DrmFourcc, DrmModifier, Handle as GemHandle, PlanarBuffer};
use drm::control::{
    self, Device as ControlDevice, Event, FbCmd2Flags, PlaneType, ResourceHandle, connector,
    framebuffer, plane, property,
};
use drm::{ClientCapability, Device as _};

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
        let created = drm_ffi::mode::dumbbuffer::create(fd, width, height, bpp * 8, 0)
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
        // SAFETY: `size` bytes just mapped, ours alone. Black to begin with.
        unsafe { std::ptr::write_bytes(map.as_ptr(), 0, size) };
        let planar = Planar {
            size: (width, height),
            format,
            modifier: None,
            pitches: [created.pitch, 0, 0, 0],
            handles: [control::from_u32(handle), None, None, None],
            offsets: [0; 4],
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
    /// Black buffer for the video plane. Some drivers refuse to enable the
    /// CRTC with no primary plane, so a UI-only modeset (the home screen
    /// before any video) may need the primary enabled with something. Freed
    /// once real video has replaced it on scanout, made again if the UI
    /// ever needs it.
    placeholder: Option<Dumb>,
    /// Video commits since the placeholder left the plane.
    placeholder_live: u32,
    warned: Warned,
}

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
    ui_pitch: u32,
    /// Frames of the phone's video presented so far; read lock-free.
    video_frames: AtomicU32,
    state: Mutex<State>,
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
            ui_pitch,
            video_frames: AtomicU32::new(0),
            state: Mutex::new(State {
                request: Request::new(),
                modeset_done: false,
                ui: ui_buf,
                band: Band::default(),
                cache: FbCache::with_capacity(CACHE_CAPACITY),
                video_held: false,
                placeholder: None,
                placeholder_live: 0,
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
        if let Some(z) = p.zpos {
            req.add(ui.id, z, 1);
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
        let flags = if wait { 0 } else { NONBLOCK };
        if self.commit(&mut st, flags).is_err() {
            return false;
        }
        st.band.clear();
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
