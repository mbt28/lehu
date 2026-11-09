// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! Input for the head unit, straight from evdev: the touchscreen, and the
//! Bluetooth keyboards and remotes paired with it. One thread polls every
//! device and the hotplug watch; what it reads leaves through a [`Sink`]
//! the main loop implements, called on that thread.
//!
//! Touch and keys go either to the screens or to the phone. The main loop
//! says which with [`Input::route`]. A contact or a key belongs to the route
//! it started on: a route change gives the old side a terminal release and
//! the new side never sees a press it has no context for.
//!
//! Keyboards are read only from Bluetooth HID devices and grabbed
//! exclusively: the touchscreen's I2C device reports keys as well and must
//! be left alone, and without the grab Magic SysRq, enabled on the image,
//! would let Alt+SysRq+B on a paired keyboard reboot the head unit.
//!
//! The pure parts, the slot machine (`touch`), the key machine (`keys`) and
//! the tables (`keymap`), take their events and their clock as arguments and
//! are tested with synthetic events; `reader` is the thread around them.

mod evdev;
mod hotplug;
mod keymap;
mod keys;
mod reader;
mod touch;

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, Ordering};
use std::thread::JoinHandle;

/// Where touch and keys go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Route {
    /// The head unit's own screens.
    #[default]
    Screens = 0,
    /// The phone's projection.
    Phone = 1,
}

impl Route {
    fn from_u8(v: u8) -> Self {
        if v == Route::Phone as u8 {
            Route::Phone
        } else {
            Route::Screens
        }
    }
}

/// Touch positions are in ten-thousandths of the panel, 0 at the left and
/// the top, swap and inversion applied: CarPlay's own unit, and what Android
/// Auto and the screens scale to their pixels with one multiply.
pub const TOUCH_SCALE: u16 = 10_000;
/// Contacts tracked at once; a contact's slot is below this.
pub const MAX_SLOTS: usize = 10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    /// Just touched.
    #[default]
    Down,
    /// Still touching, moved or not.
    Move,
    /// Just lifted; the position is where it was last.
    Up,
}

/// One finger in one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    /// The device's slot: stable for the contact, unlike the tracking id.
    pub slot: u8,
    /// 0..=TOUCH_SCALE across the panel.
    pub x: u16,
    pub y: u16,
    pub phase: Phase,
}

/// The navigation keys the screens understand, wherever they come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Up,
    Down,
    Left,
    Right,
    /// Activate the focused row, confirm.
    Ok,
    /// Leave the screen, dialog or editor.
    Back,
    /// Delete a character while editing; Back otherwise.
    Erase,
    /// Straight to the home screen.
    Home,
    /// The home screen; on it, back to the phone's waiting session.
    Menu,
    /// One rotary detent forward: the next focusable thing, wrapping.
    Next,
    /// One rotary detent back.
    Prev,
}

/// The phone's buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhoneButton {
    Up,
    Down,
    Left,
    Right,
    /// The knob's press. The only button that comes with its `Release`:
    /// CarPlay holds its knob button until then.
    Select,
    Back,
    Home,
    /// Leave the phone's video for the screens; the phone is told the head
    /// unit took the screen. When the sink says it reached a phone, the
    /// screens are brought up with a `Nav::Home` press as well.
    Menu,
    PlayPause,
    Play,
    Pause,
    NextTrack,
    PrevTrack,
    /// Siri, or the assistant.
    Siri,
    /// One wheel detent towards the user.
    RotateNext,
    RotatePrev,
}

/// The evdev value of a key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Value {
    Release = 0,
    Press = 1,
    /// Synthesised here: first after 400 ms, then every 120 ms on the
    /// screens and 150 ms on the phone. The kernel's own repeat is ignored.
    Repeat = 2,
}

/// Where input goes. Implemented by the main loop, called on the input
/// thread: every method should hand its event over (a bounded queue and a
/// wake) and return at once.
pub trait Sink {
    /// A touch frame at the end of one evdev report: every finger that is
    /// down or just lifted, one `Contact` per slot. The screens get the
    /// first finger only (they take one pointer); the phone gets them all.
    fn touch(&mut self, route: Route, contacts: &[Contact]);

    /// A key for the screens: its navigation meaning, the character it
    /// types (US layout, after Shift and Caps Lock), or both. Every press
    /// is followed by its release; arrows and Erase repeat in between.
    fn key(&mut self, nav: Option<Nav>, ch: Option<char>, value: Value);

    /// A button for the phone. Returns whether a phone took it: `Select`
    /// owes its `Release` only then, and `Menu` brings the screens up only
    /// then. Arrows come with `Repeat`; only `Select` comes with `Release`.
    fn phone(&mut self, button: PhoneButton, value: Value) -> bool;

    /// The head unit's own volume, one step up (+1) or down (-1); a held
    /// key repeats it, for 4 s at most.
    fn volume(&mut self, step: i8);

    /// The mute key: toggle.
    fn mute(&mut self);

    /// The camera key: switch to the camera, or back.
    fn camera_toggle(&mut self);

    /// Settings > Video > Camera key: the key just pressed, as its evdev
    /// code; learning is off again.
    fn key_learned(&mut self, code: u16);
}

/// What the reader needs to know at start.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The touchscreen's evdev node; empty, the first device with absolute
    /// axes that is not a Bluetooth device (a keyboard's touchpad has them).
    pub touch_device: String,
    pub swap_xy: bool,
    pub invert_x: bool,
    pub invert_y: bool,
    /// The camera key (evdev code), 0 for none.
    pub camera_key: u16,
    /// Where touch and keys go until [`Input::route`] is first called.
    pub route: Route,
}

impl Config {
    pub fn from_settings(s: &lehu_core::settings::Settings) -> Self {
        Self {
            touch_device: s.touch.device.clone(),
            swap_xy: s.touch.swap_xy,
            invert_x: s.touch.invert_x,
            invert_y: s.touch.invert_y,
            // Key codes fit; anything larger is no key and means none.
            camera_key: u16::try_from(s.camera.key).unwrap_or(0),
            route: Route::Screens,
        }
    }
}

/// What the main thread and the input thread share: three words, and the
/// descriptor that wakes the thread. No 64-bit atomics: the board has none.
pub(crate) struct Shared {
    route: AtomicU8,
    camera_key: AtomicU16,
    learning: AtomicBool,
    running: AtomicBool,
    wake: OwnedFd,
}

impl Shared {
    pub(crate) fn route(&self) -> Route {
        Route::from_u8(self.route.load(Ordering::Acquire))
    }

    pub(crate) fn camera_key(&self) -> u16 {
        self.camera_key.load(Ordering::Relaxed)
    }

    pub(crate) fn learning(&self) -> bool {
        self.learning.load(Ordering::Acquire)
    }

    /// A key was learned: learning is off until asked for again.
    pub(crate) fn learned(&self) {
        self.learning.store(false, Ordering::Release);
    }

    pub(crate) fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    pub(crate) fn wake_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: an eventfd write of exactly eight bytes from a local.
        unsafe {
            libc::write(
                self.wake.as_raw_fd(),
                (&raw const one).cast(),
                size_of::<u64>(),
            );
        }
    }

    pub(crate) fn drain_wake(&self) {
        let mut count: u64 = 0;
        // SAFETY: an eventfd read of exactly eight bytes into a local.
        unsafe {
            libc::read(
                self.wake.as_raw_fd(),
                (&raw mut count).cast(),
                size_of::<u64>(),
            );
        }
    }
}

/// The input thread, running from `start` until dropped.
pub struct Input {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Input {
    /// Opens the devices present, watches for more and starts the thread.
    /// Fails only when the wake descriptor or the thread cannot be made; a
    /// missing touchscreen or inotify is logged and lived with.
    pub fn start<S: Sink + Send + 'static>(config: Config, sink: S) -> io::Result<Self> {
        // SAFETY: eventfd takes a count and flags; a negative result is an error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor, owned here alone.
        let wake = unsafe { OwnedFd::from_raw_fd(fd) };
        let shared = Arc::new(Shared {
            route: AtomicU8::new(config.route as u8),
            camera_key: AtomicU16::new(config.camera_key),
            learning: AtomicBool::new(false),
            running: AtomicBool::new(true),
            wake,
        });
        let reader = reader::Reader::new(config, Arc::clone(&shared), sink);
        let thread = std::thread::Builder::new()
            .name("input".into())
            .spawn(move || reader.run())?;
        Ok(Self {
            shared,
            thread: Some(thread),
        })
    }

    /// Where touch and keys go from now on. Call it every tick if that is
    /// easier: only a change wakes the thread, which then releases what the
    /// old route holds.
    pub fn route(&self, route: Route) {
        let v = route as u8;
        if self.shared.route.swap(v, Ordering::AcqRel) != v {
            self.shared.wake();
        }
    }

    /// The camera key (evdev code, 0 for none), when the setting changes.
    pub fn set_camera_key(&self, code: u16) {
        self.shared.camera_key.store(code, Ordering::Relaxed);
    }

    /// Settings > Video > Camera key: while on, the next key pressed is
    /// reported through `Sink::key_learned` instead of acting, and learning
    /// ends. Esc and Back act as usual, so the screens can cancel; turn it
    /// off then.
    pub fn set_learning(&self, on: bool) {
        self.shared.learning.store(on, Ordering::Release);
    }
}

impl Drop for Input {
    fn drop(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        self.shared.wake();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(crate) mod test_sink {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Out {
        Touch(Route, Vec<Contact>),
        Key(Option<Nav>, Option<char>, Value),
        Phone(PhoneButton, Value),
        Volume(i8),
        Mute,
        Camera,
        Learned(u16),
    }

    /// Records everything; `phone_live` is what `phone` answers.
    pub struct Rec {
        pub out: Vec<Out>,
        pub phone_live: bool,
    }

    impl Rec {
        pub fn new(phone_live: bool) -> Self {
            Self {
                out: Vec::new(),
                phone_live,
            }
        }

        pub fn take(&mut self) -> Vec<Out> {
            std::mem::take(&mut self.out)
        }
    }

    impl Sink for Rec {
        fn touch(&mut self, route: Route, contacts: &[Contact]) {
            self.out.push(Out::Touch(route, contacts.to_vec()));
        }
        fn key(&mut self, nav: Option<Nav>, ch: Option<char>, value: Value) {
            self.out.push(Out::Key(nav, ch, value));
        }
        fn phone(&mut self, button: PhoneButton, value: Value) -> bool {
            self.out.push(Out::Phone(button, value));
            self.phone_live
        }
        fn volume(&mut self, step: i8) {
            self.out.push(Out::Volume(step));
        }
        fn mute(&mut self) {
            self.out.push(Out::Mute);
        }
        fn camera_toggle(&mut self) {
            self.out.push(Out::Camera);
        }
        fn key_learned(&mut self, code: u16) {
            self.out.push(Out::Learned(code));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_settings() {
        let mut s = lehu_core::settings::Settings::default();
        s.touch.swap_xy = true;
        s.camera.key = 59;
        let c = Config::from_settings(&s);
        assert!(c.swap_xy && !c.invert_x);
        assert_eq!(c.camera_key, 59);
        assert_eq!(c.route, Route::Screens);
        s.camera.key = 70_000; // no key code: none
        assert_eq!(Config::from_settings(&s).camera_key, 0);
    }

    #[test]
    fn routes_round_trip() {
        assert_eq!(Route::from_u8(Route::Phone as u8), Route::Phone);
        assert_eq!(Route::from_u8(Route::Screens as u8), Route::Screens);
        assert_eq!(Route::from_u8(9), Route::Screens);
    }
}
