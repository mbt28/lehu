// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! A V4L2 capture device -- the composite reversing camera on the F1C200s
//! TV decoder -- shown full screen on the video plane with no copy: its
//! buffers are exported as dma-bufs and scanned out as they are, so it
//! must give NV12 in contiguous memory. The camera view opens it with a
//! settings column over the picture (the input, its standard and its
//! picture levels); every change applies at once, and each input keeps its
//! own settings (`camera.inputs`).
//!
//! Everything runs on the main loop thread except the capture thread,
//! which only dequeues frames, shows them and queues them back. Ported
//! from FastCarPlay's camera.cpp with its reasons.

use std::fmt;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

use lehu_core::clock;
use lehu_core::settings::{Camera as CameraSettings, CameraInput, Standard};
use lehu_drm::{Display, Frame, Plane, Show};

mod ffi;

use ffi::{CtrlInfo, Dev};

/// Capture buffers: one on screen, the driver needs two queued (the TV
/// decoder arms the next address a frame ahead), one to spare. Three
/// still work.
const BUFFERS: u32 = 4;
const MIN_BUFFERS: u32 = 3;
/// A display that keeps refusing frames is not coming back.
const MAX_REFUSED: u32 = 10;
/// How often the signal (and, on Auto, the standard) is looked at.
const POLL_MS: u64 = 1000;
/// A standard or input change waits this long for the next press, so
/// cycling through them restarts the stream once.
const RESTART_DELAY_MS: u64 = 300;
/// Changes are saved this long after the last one (NAND writes).
const SAVE_DELAY_MS: u64 = 2000;
/// The screens stay up at most this long for the first frame (the decoder
/// locking onto the signal), then give way to whatever the plane has.
const FIRST_FRAME_MS: u64 = 1000;
/// One saved profile per input, at most.
const MAX_INPUTS: usize = 4;

const STD_PAL: u64 = 0xff;
const STD_NTSC_M: u64 = 0x1000;
const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// A picture level (or a menu control) as the column shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Level {
    pub label: &'static str,
    pub value: String,
}

/// What to write back to the settings: the input shown and every input's
/// profile. Handed out by `tick` 2 s after the last change, and by `stop`.
#[derive(Clone, Debug, PartialEq)]
pub struct Save {
    pub input: u32,
    pub inputs: Vec<CameraInput>,
}

/// The controls the column offers, in its order: standard ones by id, the
/// TV decoder's private ones by name. Nothing else is ever touched: some of
/// the decoder's controls make noise or misbehave (burst gates, comb
/// filter).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    Brightness,
    Contrast,
    Saturation,
    Hue,
    Sharpness,
    ChromaGain,
    ChromaEnhance,
}

const CID_BASE: u32 = 0x0098_0900;

impl Key {
    const ALL: [Key; 7] = [
        Key::Brightness,
        Key::Contrast,
        Key::Saturation,
        Key::Hue,
        Key::Sharpness,
        Key::ChromaGain,
        Key::ChromaEnhance,
    ];

    /// The V4L2 id, or the name to look for when there is no fixed id.
    fn lookup(self) -> (u32, Option<&'static str>) {
        match self {
            Key::Brightness => (CID_BASE, None),
            Key::Contrast => (CID_BASE + 1, None),
            Key::Saturation => (CID_BASE + 2, None),
            Key::Hue => (CID_BASE + 3, None),
            Key::Sharpness => (CID_BASE + 27, None),
            Key::ChromaGain => (CID_BASE + 36, Some("Chroma Gain")),
            Key::ChromaEnhance => (0, Some("Chroma Enhance")),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Key::Brightness => "Brightness",
            Key::Contrast => "Contrast",
            Key::Saturation => "Saturation",
            Key::Hue => "Hue",
            Key::Sharpness => "Sharpness",
            Key::ChromaGain => "Chroma gain",
            Key::ChromaEnhance => "Chroma enh.",
        }
    }

    fn get(self, p: &CameraInput) -> Option<i32> {
        match self {
            Key::Brightness => p.brightness,
            Key::Contrast => p.contrast,
            Key::Saturation => p.saturation,
            Key::Hue => p.hue,
            Key::Sharpness => p.sharpness,
            Key::ChromaGain => p.chroma_gain,
            Key::ChromaEnhance => p.chroma_enhance,
        }
    }

    fn set(self, p: &mut CameraInput, value: i32) {
        let slot = match self {
            Key::Brightness => &mut p.brightness,
            Key::Contrast => &mut p.contrast,
            Key::Saturation => &mut p.saturation,
            Key::Hue => &mut p.hue,
            Key::Sharpness => &mut p.sharpness,
            Key::ChromaGain => &mut p.chroma_gain,
            Key::ChromaEnhance => &mut p.chroma_enhance,
        };
        *slot = Some(value);
    }
}

struct Control {
    id: u32,
    key: Key,
    min: i32,
    max: i32,
    step: i32,
    def: i32,
    value: i32,
    /// A menu: `items` are its entries.
    items: Vec<(i32, String)>,
}

impl Control {
    fn is_menu(&self) -> bool {
        !self.items.is_empty()
    }
}

/// The standard a profile asks for; 0 = Auto (whatever the input carries).
fn wanted(standard: Standard) -> u64 {
    match standard {
        Standard::Auto => 0,
        Standard::Pal => STD_PAL,
        Standard::Ntsc => STD_NTSC_M,
    }
}

/// A detection can match several (625 lines: PAL and PAL-Nc); plain PAL /
/// NTSC win.
fn plain(std: u64) -> u64 {
    if std & STD_NTSC_M != 0 {
        STD_NTSC_M
    } else if std & STD_PAL != 0 {
        STD_PAL
    } else {
        std
    }
}

fn standard_label(standard: Standard) -> &'static str {
    match standard {
        Standard::Auto => "Auto",
        Standard::Pal => "PAL",
        Standard::Ntsc => "NTSC",
    }
}

/// Auto -> PAL -> NTSC (direction +1) and back.
fn cycle(standard: Standard, direction: i32) -> Standard {
    let order = [Standard::Auto, Standard::Pal, Standard::Ntsc];
    let at = order.iter().position(|&s| s == standard).unwrap_or(0);
    let next = if direction > 0 { at + 1 } else { at + 2 } % 3;
    order[next]
}

/// One step of a level: 5 % of the range, in whole steps of the control.
fn stride(min: i32, max: i32, step: i32) -> i32 {
    let step = step.max(1);
    step.max(((max - min) / 20 / step) * step)
}

/// A level as text: a percentage of the range, or an offset either way for
/// a centred one (hue).
fn level_text(min: i32, max: i32, value: i32) -> String {
    if min < 0 {
        let span = i64::from((-min).max(max)).max(1);
        format!("{:+}%", i64::from(value) * 100 / span)
    } else {
        let span = i64::from(max - min).max(1);
        format!("{}%", (i64::from(value - min) * 100 + span / 2) / span)
    }
}

fn due(now: u64, at: Option<u64>) -> bool {
    at.is_some_and(|t| now >= t)
}

/// Why the camera could not start or stopped, as the view shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error {
    pub text: String,
    /// For lack of video memory: with the phone's decoder parked it may
    /// start.
    pub no_memory: bool,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn err(text: impl Into<String>) -> Error {
    Error {
        text: text.into(),
        no_memory: false,
    }
}

/// The configured device, or else the first capture device (decoders are
/// memory-to-memory and skipped).
fn open_device(device: &str) -> Result<Dev, Error> {
    if !device.is_empty() {
        return Dev::open(device).map_err(|e| {
            err(if e == libc::ENOENT {
                format!("No camera at {device}")
            } else if e == libc::ENODEV {
                format!("{device} is not a camera")
            } else {
                format!("Cannot open {device}")
            })
        });
    }
    for i in 0..16 {
        if let Ok(dev) = Dev::open(&format!("/dev/video{i}")) {
            return Ok(dev);
        }
    }
    Err(err("No camera found"))
}

/// Is there a camera at all (opens the device for a moment).
pub fn present(device: &str) -> bool {
    open_device(device).is_ok()
}

/// The capture thread's share: the descriptor and the exported buffers,
/// valid until the thread is joined.
struct Capture {
    fd: i32,
    dmabufs: Vec<i32>,
    width: u32,
    height: u32,
    pitch: u32,
    run: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    first_shown: Arc<AtomicBool>,
    display: Arc<Display>,
}

pub struct Camera {
    display: Arc<Display>,
    dev: Option<Dev>,
    dmabufs: Vec<OwnedFd>,
    width: u32,
    height: u32,
    pitch: u32,
    /// On the video plane, the decoder's frames held off.
    streaming: bool,
    thread: Option<JoinHandle<()>>,
    run: Arc<AtomicBool>,
    failed: Arc<AtomicBool>,
    first_shown: Arc<AtomicBool>,
    start_ms: u64,
    error: String,
    no_memory: bool,
    /// `hold_video(true)` is ours to undo.
    held: bool,

    inputs: Vec<String>,
    controls: Vec<Control>,
    profiles: Vec<CameraInput>,
    input: usize,
    /// The current input takes a video standard.
    has_std: bool,
    /// The one set.
    std: u64,
    no_signal: bool,
    restart_at: Option<u64>,
    restart_std: u64,
    poll_at: u64,
    unsaved: bool,
    save_at: u64,
    levels: Vec<Level>,
    generation: u32,
}

impl Camera {
    pub fn new(display: Arc<Display>) -> Self {
        Self {
            display,
            dev: None,
            dmabufs: Vec::new(),
            width: 0,
            height: 0,
            pitch: 0,
            streaming: false,
            thread: None,
            run: Arc::new(AtomicBool::new(false)),
            failed: Arc::new(AtomicBool::new(false)),
            first_shown: Arc::new(AtomicBool::new(false)),
            start_ms: 0,
            error: String::new(),
            no_memory: false,
            held: false,
            inputs: Vec::new(),
            controls: Vec::new(),
            profiles: Vec::new(),
            input: 0,
            has_std: false,
            std: 0,
            no_signal: false,
            restart_at: None,
            restart_std: 0,
            poll_at: 0,
            unsaved: false,
            save_at: 0,
            levels: Vec::new(),
            generation: 0,
        }
    }

    pub fn running(&self) -> bool {
        self.dev.is_some()
    }

    /// The stream broke after a good start; `error` says how.
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn error(&self) -> &str {
        &self.error
    }

    /// The last start or restart failed for lack of video memory.
    pub fn out_of_memory(&self) -> bool {
        self.no_memory
    }

    /// Its first frame is on the video plane since `start`, or it has had
    /// long enough: what covers the plane can give way.
    pub fn picture_ready(&self) -> bool {
        self.first_shown.load(Ordering::Acquire)
            || clock::now_ms().saturating_sub(self.start_ms) >= FIRST_FRAME_MS
    }

    pub fn inputs(&self) -> &[String] {
        &self.inputs
    }

    pub fn input(&self) -> usize {
        self.input
    }

    pub fn no_signal(&self) -> bool {
        self.no_signal
    }

    pub fn has_standard(&self) -> bool {
        self.has_std
    }

    pub fn standard_label(&self) -> &'static str {
        standard_label(self.profile().standard)
    }

    pub fn levels(&self) -> &[Level] {
        &self.levels
    }

    /// Changes whenever anything the column shows does.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    fn profile(&self) -> &CameraInput {
        static DEFAULT: std::sync::OnceLock<CameraInput> = std::sync::OnceLock::new();
        self.profiles
            .get(self.input)
            .unwrap_or_else(|| DEFAULT.get_or_init(CameraInput::default))
    }

    fn profile_mut(&mut self) -> &mut CameraInput {
        let at = self.input.min(MAX_INPUTS - 1);
        while self.profiles.len() <= at {
            self.profiles.push(CameraInput::default());
        }
        &mut self.profiles[at]
    }

    /// Opens the device with the last input's settings and streams onto the
    /// video plane, keeping the decoder's frames off it.
    pub fn start(&mut self, settings: &CameraSettings) -> Result<(), Error> {
        if self.dev.is_some() {
            return Ok(());
        }
        self.error.clear();
        self.failed.store(false, Ordering::Release);
        self.no_memory = false;
        self.first_shown.store(false, Ordering::Release);
        self.start_ms = clock::now_ms();
        let dev = match open_device(&settings.device) {
            Ok(d) => d,
            Err(e) => {
                self.error = e.text.clone();
                return Err(e);
            }
        };
        self.dev = Some(dev);

        self.inputs.clear();
        for i in 0..8u32 {
            match self.dev().enum_input(i) {
                Some(info) => self.inputs.push(info.name),
                None => break,
            }
        }
        self.inputs.truncate(MAX_INPUTS); // one saved profile each
        self.profiles = settings.inputs.clone();
        self.profiles.truncate(MAX_INPUTS);
        while self.profiles.len() < self.inputs.len() {
            self.profiles.push(CameraInput::default());
        }
        self.input = (settings.input as usize).min(self.inputs.len().saturating_sub(1));
        self.restart_at = None;
        self.no_signal = false;
        self.unsaved = false;
        self.scan_controls();
        let want = wanted(self.profile().standard);
        self.configure(want);
        if let Err(e) = self.stream_on() {
            log::warn!("camera: {e}");
            self.stop();
            self.error = e.text.clone();
            self.no_memory = e.no_memory;
            return Err(e);
        }
        self.poll_at = clock::now_ms() + POLL_MS;
        self.changed(false);
        Ok(())
    }

    /// Lets go of the device and the plane; what is not saved yet comes
    /// back for the caller to persist.
    pub fn stop(&mut self) -> Option<Save> {
        self.dev.as_ref()?;
        let save = self.unsaved.then(|| self.save());
        self.stream_off(true);
        self.levels.clear();
        self.generation = self.generation.wrapping_add(1);
        save
    }

    fn dev(&self) -> &Dev {
        self.dev
            .as_ref()
            .expect("the device is open while streaming")
    }

    fn scan_controls(&mut self) {
        self.controls.clear();
        let dev = self.dev.as_ref().expect("open");
        for key in Key::ALL {
            let (id, name) = key.lookup();
            let mut info: Option<CtrlInfo> = if id != 0 { dev.query_ctrl(id) } else { None };
            if info.is_none()
                && let Some(name) = name
                && let Some(found) = dev.find_ctrl(name)
            {
                info = dev.query_ctrl(found);
            }
            let Some(q) = info else { continue };
            if q.disabled() || q.read_only() || q.max <= q.min {
                continue;
            }
            let mut items = Vec::new();
            if q.is_menu() {
                for i in q.min..=q.max {
                    if let Some(name) = dev.query_menu(q.id, i as u32) {
                        items.push((i, name));
                    }
                }
                if items.is_empty() {
                    continue;
                }
            }
            self.controls.push(Control {
                id: q.id,
                key,
                min: q.min,
                max: q.max,
                step: q.step.max(1),
                def: q.def,
                value: q.def,
                items,
            });
        }
    }

    /// Input, standard (0 = Auto: detect, blocking -- only on the first
    /// open, when the screen is black anyway), then the levels: a standard
    /// change resets some of them on the TV decoder. Buffers must be freed:
    /// S_STD is refused while any exist.
    fn configure(&mut self, mut std: u64) {
        let input = self.input;
        let dev = self.dev.as_ref().expect("open");
        if let Err(e) = dev.set_input(input as i32)
            && e != libc::ENOTTY
        {
            log::warn!("camera: input {input}: {}", errno_text(e));
        }
        self.has_std = dev.enum_input(input as u32).is_some_and(|i| i.has_std);
        if self.has_std {
            // The decoder only locks a few hundred ms after it is powered up.
            for _ in 0..8 {
                if std != 0 {
                    break;
                }
                match dev.query_std() {
                    Some(seen) if seen != 0 => std = plain(seen),
                    _ => thread::sleep(std::time::Duration::from_millis(100)),
                }
            }
            if std != 0
                && let Err(e) = dev.set_std(std)
            {
                log::warn!("camera: set standard: {}", errno_text(e));
            }
            self.std = dev.get_std().unwrap_or(0);
        }
        self.apply_profile();
    }

    /// Every control to the current input's value, or to the control's
    /// default (read again: it can follow the standard). Never the device's
    /// current value: that would carry one input's tuning over to another.
    fn apply_profile(&mut self) {
        let profile = self.profile().clone();
        let dev = self.dev.as_ref().expect("open");
        for c in &mut self.controls {
            if let Some(q) = dev.query_ctrl(c.id) {
                c.def = q.def;
            }
            let value = c.key.get(&profile).unwrap_or(c.def).clamp(c.min, c.max);
            if let Err(e) = dev.set_ctrl(c.id, value) {
                log::warn!("camera: {} = {value}: {}", c.key.label(), errno_text(e));
            }
            c.value = dev.get_ctrl(c.id).unwrap_or(c.def);
        }
    }

    /// The format the standard gives, buffers exported for the display,
    /// stream on, and the capture thread.
    fn stream_on(&mut self) -> Result<(), Error> {
        let dev = self.dev.as_ref().expect("open");
        let (width, height, pitch) = dev.set_nv12().map_err(|e| {
            err(if e == libc::EINVAL {
                "Camera has no NV12"
            } else {
                "Camera format unknown"
            })
        })?;
        self.width = width;
        self.height = height;
        self.pitch = pitch;
        let count = match dev.reqbufs(BUFFERS) {
            Ok(n) if n >= MIN_BUFFERS => n,
            _ => {
                return Err(Error {
                    text: "Not enough video memory for the camera".into(),
                    no_memory: true,
                });
            }
        };
        for i in 0..count {
            let fd = dev
                .expbuf(i)
                .map_err(|_| err("Camera buffers cannot be shared"))?;
            self.dmabufs.push(fd);
            dev.qbuf(i).map_err(|_| err("Camera refused a buffer"))?;
        }
        if let Err(e) = dev.stream(true) {
            // The TV decoder's 3D comb filter (off by default) takes 2 MiB
            // more while streaming, from the memory the phone's video
            // decoder also uses: without it the picture is a little softer,
            // but there.
            let comb = if e == libc::ENOMEM {
                dev.find_ctrl("Comb Filter 3D")
            } else {
                None
            };
            let again =
                comb.is_some_and(|id| dev.set_ctrl(id, 0).is_ok()) && dev.stream(true).is_ok();
            if !again {
                return Err(Error {
                    text: if e == libc::ENOMEM {
                        "Not enough video memory for the camera".into()
                    } else {
                        format!("Camera does not start: {}", errno_text(e))
                    },
                    no_memory: e == libc::ENOMEM,
                });
            }
            log::info!("camera: comb filter off, not enough memory for it");
        }
        if !self.held {
            self.display.hold_video(true);
            self.held = true;
        }
        self.streaming = true;
        log::info!(
            "camera: {width}x{height} NV12, {count} buffers, input {}",
            self.input
        );
        self.run.store(true, Ordering::Release);
        let capture = Capture {
            fd: dev.as_raw_fd(),
            dmabufs: self.dmabufs.iter().map(AsRawFd::as_raw_fd).collect(),
            width,
            height,
            pitch,
            run: Arc::clone(&self.run),
            failed: Arc::clone(&self.failed),
            first_shown: Arc::clone(&self.first_shown),
            display: Arc::clone(&self.display),
        };
        let thread = thread::Builder::new()
            .name("camera".into())
            .spawn(move || capture.run())
            .map_err(|e| err(format!("Camera thread: {e}")))?;
        self.thread = Some(thread);
        Ok(())
    }

    /// The stream and its buffers; with `release`, also the device and the
    /// plane. Without, the device stays open: closing it powers the decoder
    /// down, and it would have to lock again.
    fn stream_off(&mut self, release: bool) {
        self.run.store(false, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        if let Some(dev) = &self.dev {
            if self.streaming {
                let _ = dev.stream(false);
                // A buffer may still be on scanout: the display lets go of
                // them first.
                self.display.flush_video();
                self.streaming = false;
            }
            self.dmabufs.clear();
            let _ = dev.reqbufs(0);
        }
        if !release {
            return;
        }
        if self.held {
            self.display.hold_video(false);
            self.held = false;
        }
        self.dev = None;
    }

    fn restart(&mut self) {
        self.no_memory = false;
        self.stream_off(false);
        let std = self.restart_std;
        self.configure(std);
        if let Err(e) = self.stream_on() {
            self.no_memory = e.no_memory;
            self.fail(e.text);
        }
        self.changed(false);
    }

    fn fail(&mut self, why: String) {
        log::warn!("camera: {why}");
        self.error = why;
        self.failed.store(true, Ordering::Release);
    }

    /// Every loop turn while running: a due restart, the once-a-second
    /// check of the signal (and, on Auto, of the standard), the delayed
    /// save, handed out when due.
    pub fn tick(&mut self) -> Option<Save> {
        if self.dev.is_none() || self.failed() {
            return None;
        }
        let now = clock::now_ms();
        if due(now, self.restart_at) {
            self.restart_at = None;
            self.restart();
            self.poll_at = now + POLL_MS;
        }
        if now >= self.poll_at {
            self.poll_at = now + POLL_MS;
            let dev = self.dev.as_ref().expect("open");
            if let Some(info) = dev.enum_input(self.input as u32)
                && info.no_signal != self.no_signal
            {
                self.no_signal = info.no_signal;
                self.changed(false);
            }
            // Auto follows the input: a reversing camera often powers up
            // only in reverse, after the view is open.
            if self.has_std
                && wanted(self.profile().standard) == 0
                && !self.no_signal
                && self.restart_at.is_none()
                && let Some(seen) = self.dev().query_std()
                && seen != 0
                && plain(seen) & self.std == 0
            {
                self.restart_std = plain(seen);
                self.restart_at = Some(now);
            }
        }
        if self.unsaved && now >= self.save_at {
            return Some(self.save());
        }
        None
    }

    /// Live: the levels follow at once; the stream restarts only for a
    /// standard it is not in.
    pub fn select_input(&mut self, input: usize) {
        if self.dev.is_none() || input >= self.inputs.len() || input == self.input {
            return;
        }
        let now = clock::now_ms();
        self.input = input;
        let dev = self.dev.as_ref().expect("open");
        let _ = dev.set_input(input as i32);
        self.has_std = dev.enum_input(input as u32).is_some_and(|i| i.has_std);
        let want = wanted(self.profile().standard);
        if self.has_std && want != 0 && want != self.std {
            self.restart_std = want;
            self.restart_at = Some(now + RESTART_DELAY_MS);
        } else {
            self.apply_profile();
            self.poll_at = now + 400; // Auto: look at the new input soon
        }
        self.no_signal = false;
        self.changed(true);
    }

    /// Auto -> PAL -> NTSC (direction +1) and back.
    pub fn cycle_standard(&mut self, direction: i32) {
        if self.dev.is_none() || !self.has_std || direction == 0 {
            return;
        }
        let now = clock::now_ms();
        let next = cycle(self.profile().standard, direction);
        self.profile_mut().standard = next;
        let want = wanted(next);
        if want != 0 && want != self.std {
            self.restart_std = want;
            self.restart_at = Some(now + RESTART_DELAY_MS);
        } else {
            self.restart_at = None;
            self.poll_at = now; // Auto: check the input now
        }
        self.changed(true);
    }

    /// Level `level` one step up (+1) or down (-1), applied at once.
    pub fn adjust_level(&mut self, level: usize, direction: i32) {
        if self.dev.is_none() || level >= self.controls.len() || direction == 0 {
            return;
        }
        let step = if direction > 0 { 1 } else { -1 };
        let c = &self.controls[level];
        let value = if c.is_menu() {
            let i = c.items.iter().position(|(v, _)| *v == c.value).unwrap_or(0) as i32;
            let at = (i + step).clamp(0, c.items.len() as i32 - 1);
            c.items[at as usize].0
        } else {
            (c.value + step * stride(c.min, c.max, c.step)).clamp(c.min, c.max)
        };
        let (id, key, def) = (c.id, c.key, c.def);
        let dev = self.dev.as_ref().expect("open");
        if let Err(e) = dev.set_ctrl(id, value) {
            log::warn!("camera: {} = {value}: {}", key.label(), errno_text(e));
        }
        let now_value = dev.get_ctrl(id).unwrap_or(def);
        self.controls[level].value = now_value;
        key.set(self.profile_mut(), now_value);
        self.changed(true);
    }

    fn build_levels(&mut self) {
        self.levels.clear();
        for c in &self.controls {
            let value = if c.is_menu() {
                c.items
                    .iter()
                    .find(|(v, _)| *v == c.value)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default()
            } else {
                level_text(c.min, c.max, c.value)
            };
            self.levels.push(Level {
                label: c.key.label(),
                value,
            });
        }
    }

    fn changed(&mut self, save: bool) {
        self.build_levels();
        self.generation = self.generation.wrapping_add(1);
        if save {
            self.unsaved = true;
            self.save_at = clock::now_ms() + SAVE_DELAY_MS;
        }
    }

    fn save(&mut self) -> Save {
        self.unsaved = false;
        Save {
            input: self.input as u32,
            inputs: self.profiles.clone(),
        }
    }
}

impl Drop for Camera {
    fn drop(&mut self) {
        self.stream_off(true);
    }
}

impl Capture {
    fn run(self) {
        let mut shown: Option<u32> = None; // on scanout until the next replaces it
        let mut refused = 0;
        let fail = |why: &str| {
            log::warn!("camera: {why}");
            self.failed.store(true, Ordering::Release);
        };
        while self.run.load(Ordering::Acquire) {
            let mut p = libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd of the stated count.
            let r = unsafe { libc::poll(&mut p, 1, 200) };
            if r < 0 {
                if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                fail("Camera stopped");
                break;
            }
            if r == 0 {
                continue;
            }
            let index = match ffi::dqbuf(self.fd) {
                Ok(i) => i,
                Err(e) if e == libc::EAGAIN => continue,
                Err(_) => {
                    fail("Camera stopped");
                    break;
                }
            };
            let Some(&fd) = self.dmabufs.get(index as usize) else {
                let _ = ffi::qbuf(self.fd, index);
                continue;
            };
            let plane = |offset: u32| Plane {
                fd,
                pitch: self.pitch,
                offset,
            };
            let frame = Frame {
                fourcc: FOURCC_NV12,
                width: self.width,
                height: self.height,
                planes: [
                    plane(0),
                    plane(self.pitch * self.height),
                    Plane::default(),
                    Plane::default(),
                ],
                plane_count: 2,
                modifier: 0,
            };
            match self.display.show_camera(&frame) {
                Show::Shown => {
                    if let Some(prev) = shown {
                        let _ = ffi::qbuf(self.fd, prev);
                    }
                    shown = Some(index);
                    refused = 0;
                    self.first_shown.store(true, Ordering::Release);
                }
                // A flip still pending: this frame is skipped.
                Show::Busy => {
                    let _ = ffi::qbuf(self.fd, index);
                }
                Show::Failed => {
                    let _ = ffi::qbuf(self.fd, index);
                    refused += 1;
                    if refused >= MAX_REFUSED {
                        fail("The display cannot show this camera");
                        break;
                    }
                }
            }
        }
    }
}

fn errno_text(e: i32) -> String {
    std::io::Error::from_raw_os_error(e).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standards() {
        assert_eq!(plain(STD_PAL | 0x100), STD_PAL);
        assert_eq!(plain(STD_NTSC_M | STD_PAL), STD_NTSC_M);
        assert_eq!(plain(0), 0);
        assert_eq!(cycle(Standard::Auto, 1), Standard::Pal);
        assert_eq!(cycle(Standard::Ntsc, 1), Standard::Auto);
        assert_eq!(cycle(Standard::Auto, -1), Standard::Ntsc);
        assert_eq!(wanted(Standard::Pal), STD_PAL);
        assert_eq!(standard_label(Standard::Ntsc), "NTSC");
    }

    #[test]
    fn level_steps_and_text() {
        assert_eq!(stride(0, 255, 1), 12);
        assert_eq!(stride(0, 10, 1), 1);
        assert_eq!(stride(0, 100, 5), 5);
        assert_eq!(level_text(0, 255, 255), "100%");
        assert_eq!(level_text(0, 255, 0), "0%");
        assert_eq!(level_text(0, 255, 128), "50%");
        assert_eq!(level_text(-128, 127, 0), "+0%");
        assert_eq!(level_text(-128, 127, -64), "-50%");
        assert_eq!(level_text(-128, 127, 127), "+99%");
    }

    #[test]
    fn keys_map_to_the_profile() {
        let mut p = CameraInput::default();
        for key in Key::ALL {
            assert_eq!(key.get(&p), None);
            key.set(&mut p, 7);
            assert_eq!(key.get(&p), Some(7));
        }
        assert_eq!(p.chroma_enhance, Some(7));
        assert_eq!(Key::ChromaGain.lookup().1, Some("Chroma Gain"));
        assert_eq!(Key::Brightness.lookup().0, CID_BASE);
    }

    #[test]
    fn no_camera_is_an_error_with_a_reason() {
        let Err(e) = open_device("/dev/lehu-no-such-video") else {
            panic!("opened");
        };
        assert_eq!(e.text, "No camera at /dev/lehu-no-such-video");
        assert!(!e.no_memory);
    }
}
