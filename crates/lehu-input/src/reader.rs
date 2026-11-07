//! The input thread: every device and the hotplug watch in one `poll`, which
//! sleeps until an event, a repeat or a retry is due, or the main thread
//! wakes it for a route change.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;

use lehu_core::clock;

use crate::evdev::{self, RawEvent, Read, has};
use crate::hotplug::{self, NodeName, Notice, PATH_BUF};
use crate::keymap;
use crate::keys::{Controls, Keyboard, Keys};
use crate::touch::{Calibration, Snapshot, Touch};
use crate::{Config, MAX_SLOTS, Shared, Sink};

/// Keyboards and remotes open at once.
const MAX_KEYBOARDS: usize = 8;
/// Nodes waiting to be opened again.
const MAX_RETRIES: usize = 8;
/// Events taken from a device in one read.
const BATCH: usize = 64;
/// devtmpfs creates a node before it is always openable, and udev gives it
/// to the input group a moment later -- several seconds on a busy board.
const OPEN_RETRY_MS: u64 = 100;
const OPEN_RETRY_FOR_MS: u64 = 10_000;

const INPUT_DIR: &CStr = c"/dev/input";
const DEV_DIR: &CStr = c"/dev";

struct TouchDev {
    file: File,
    node: NodeName,
    state: Touch,
}

struct KeyDev {
    file: File,
    node: NodeName,
    kb: Keyboard,
}

#[derive(Clone, Copy)]
struct Retry {
    node: NodeName,
    next: u64,
    until: u64,
}

pub(crate) struct Reader<S: Sink> {
    sink: S,
    shared: Arc<Shared>,
    config: Config,
    /// The configured touchscreen: its path, and its name for hotplug.
    /// None: the first device with absolute axes.
    touch_path: Option<CString>,
    touch_name: Option<NodeName>,
    inotify: Option<OwnedFd>,
    /// /dev: for /dev/input coming back.
    dev_wd: i32,
    /// /dev/input: nodes coming and going.
    input_wd: i32,
    touch: Option<TouchDev>,
    keyboards: Vec<KeyDev>,
    keys: Keys,
    controls: Controls,
    retries: Vec<Retry>,
    events: [RawEvent; BATCH],
    fds: Vec<libc::pollfd>,
    // Where each descriptor sits in `fds` for this turn.
    inotify_at: Option<usize>,
    touch_at: Option<usize>,
    keyboards_from: usize,
}

impl<S: Sink> Reader<S> {
    pub(crate) fn new(config: Config, shared: Arc<Shared>, sink: S) -> Self {
        let (touch_path, touch_name) = if config.touch_device.is_empty() {
            (None, None)
        } else {
            let path = config.touch_device.as_str();
            let name = path
                .rsplit('/')
                .next()
                .and_then(|n| NodeName::new(n.as_bytes()));
            (CString::new(path).ok(), name)
        };
        let keys = Keys::new(shared.route());
        let controls = Controls {
            camera_key: shared.camera_key(),
            learning: shared.learning(),
        };
        let mut reader = Self {
            sink,
            shared,
            config,
            touch_path,
            touch_name,
            inotify: None,
            dev_wd: -1,
            input_wd: -1,
            touch: None,
            keyboards: Vec::with_capacity(MAX_KEYBOARDS),
            keys,
            controls,
            retries: Vec::with_capacity(MAX_RETRIES),
            events: [RawEvent::default(); BATCH],
            fds: Vec::with_capacity(3 + MAX_KEYBOARDS),
            inotify_at: None,
            touch_at: None,
            keyboards_from: 0,
        };
        reader.start_watch();
        reader
    }

    fn start_watch(&mut self) {
        // SAFETY: inotify_init1 takes flags only.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            log::warn!(
                "input: no inotify ({}); devices connected later are missed",
                io::Error::last_os_error()
            );
            return;
        }
        // SAFETY: a fresh descriptor, owned here alone.
        self.inotify = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        // /dev as well: /dev/input may not exist yet, or come and go.
        // SAFETY: the path is a valid C string; the call reads it only.
        self.dev_wd = unsafe {
            libc::inotify_add_watch(fd, DEV_DIR.as_ptr(), libc::IN_CREATE | libc::IN_ONLYDIR)
        };
        if !self.watch_input_dir() && self.dev_wd < 0 {
            log::warn!(
                "input: cannot watch /dev/input ({}); devices connected later are missed",
                io::Error::last_os_error()
            );
            self.inotify = None;
        }
    }

    /// (Re)arms the /dev/input watch; false while the directory is missing.
    fn watch_input_dir(&mut self) -> bool {
        let Some(fd) = self.inotify.as_ref().map(AsRawFd::as_raw_fd) else {
            return false;
        };
        // SAFETY: the path is a valid C string; the call reads it only.
        self.input_wd = unsafe {
            libc::inotify_add_watch(
                fd,
                INPUT_DIR.as_ptr(),
                libc::IN_CREATE | libc::IN_ATTRIB | libc::IN_DELETE,
            )
        };
        self.input_wd >= 0
    }

    pub(crate) fn run(mut self) {
        self.open_configured_touch();
        self.scan_existing(clock::now_ms());
        while self.shared.running() {
            self.sync_controls();
            let now = clock::now_ms();
            let timeout = self.timers(now).map_or(-1, |due| {
                due.saturating_sub(now).min(i32::MAX as u64) as libc::c_int
            });
            self.fill_fds();
            // SAFETY: fds holds fds.len() valid pollfd entries for the call.
            let n = unsafe {
                libc::poll(
                    self.fds.as_mut_ptr(),
                    self.fds.len() as libc::nfds_t,
                    timeout,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                log::error!("input: poll failed ({e}); no more input");
                break;
            }
            if self.fds[0].revents & libc::POLLIN != 0 {
                self.shared.drain_wake();
            }
            if !self.shared.running() {
                break;
            }
            // Before any key: a press belongs to the current route.
            self.sync_controls();
            let now = clock::now_ms();
            self.read_keyboards(now);
            self.read_touch();
            self.read_inotify(now);
        }
        self.keys.release_all(&mut self.sink);
        if let Some(t) = &mut self.touch {
            t.state.device_gone(&mut self.sink);
        }
    }

    /// The route and the controls, from the main thread.
    fn sync_controls(&mut self) {
        let route = self.shared.route();
        self.keys.set_route(route, &mut self.sink);
        if let Some(t) = &mut self.touch {
            t.state.set_route(route, &mut self.sink);
        }
        self.controls.camera_key = self.shared.camera_key();
        self.controls.learning = self.shared.learning();
    }

    /// Repeats, owed wheel turns and open retries that are due; when the
    /// next one is.
    fn timers(&mut self, now: u64) -> Option<u64> {
        let mut next = self.keys.tick(now, &mut self.sink);
        let mut i = 0;
        while i < self.retries.len() {
            let r = self.retries[i];
            if now >= r.next {
                if self.consider(&r.node, now) {
                    self.retries.swap_remove(i);
                    continue;
                }
                if now >= r.until {
                    // Only a node its group may read says the user is not
                    // in that group; one that is still root's alone waits
                    // for udev, and the attribute watch opens it then.
                    if group_may_read(&r.node) {
                        log::warn!(
                            "input: {}: cannot open; is the user in the input group?",
                            r.node
                        );
                    } else {
                        log::info!(
                            "input: {}: not given to the input group yet; opened when it is",
                            r.node
                        );
                    }
                    self.retries.swap_remove(i);
                    continue;
                }
                self.retries[i].next = now + OPEN_RETRY_MS;
            }
            let due = self.retries[i].next;
            next = Some(next.map_or(due, |d| d.min(due)));
            i += 1;
        }
        next
    }

    fn fill_fds(&mut self) {
        let poll_in = |fd: RawFd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        self.fds.clear();
        self.fds.push(poll_in(self.shared.wake_fd()));
        self.inotify_at = self.inotify.as_ref().map(|f| {
            self.fds.push(poll_in(f.as_raw_fd()));
            self.fds.len() - 1
        });
        self.touch_at = self.touch.as_ref().map(|t| {
            self.fds.push(poll_in(t.file.as_raw_fd()));
            self.fds.len() - 1
        });
        self.keyboards_from = self.fds.len();
        for k in &self.keyboards {
            self.fds.push(poll_in(k.file.as_raw_fd()));
        }
    }

    fn gone(revents: libc::c_short) -> bool {
        revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
    }

    fn read_keyboards(&mut self, now: u64) {
        // Back to front: dropping one shifts only those after it, whose
        // slots in `fds` were matched already.
        for i in (0..self.keyboards.len()).rev() {
            let revents = self.fds[self.keyboards_from + i].revents;
            if revents == 0 {
                continue;
            }
            if Self::gone(revents) || !self.read_keyboard(i, now) {
                self.drop_keyboard(i);
            }
        }
    }

    /// Drains one keyboard; false when it is gone.
    fn read_keyboard(&mut self, i: usize, now: u64) -> bool {
        let Self {
            keyboards,
            events,
            keys,
            controls,
            sink,
            shared,
            ..
        } = self;
        let dev = &mut keyboards[i];
        let fd = dev.file.as_raw_fd();
        loop {
            match evdev::read_events(fd, &mut events[..]) {
                Read::Events(n) => {
                    let learning = controls.learning;
                    for ev in &events[..n] {
                        if keys.event(&mut dev.kb, ev, now, controls, &mut *sink) {
                            let mut state = [0usize; evdev::KEY_WORDS];
                            if evdev::key_state(fd, &mut state).is_err() {
                                state = [0; evdev::KEY_WORDS];
                            }
                            keys.resync(&mut dev.kb, &state, now, &mut *sink);
                        }
                    }
                    if learning && !controls.learning {
                        shared.learned();
                    }
                }
                Read::WouldBlock => return true,
                Read::Gone => return false,
            }
        }
    }

    fn drop_keyboard(&mut self, i: usize) {
        let dev = self.keyboards.remove(i);
        self.keys.device_gone(dev.kb.id, &mut self.sink);
        log::info!("input: - {}", dev.node);
    }

    fn read_touch(&mut self) {
        let Some(at) = self.touch_at else {
            return;
        };
        let revents = self.fds[at].revents;
        if revents == 0 {
            return;
        }
        if Self::gone(revents) || !self.read_touch_events() {
            self.drop_touch();
        }
    }

    /// Drains the touchscreen; false when it is gone.
    fn read_touch_events(&mut self) -> bool {
        let Self {
            touch,
            events,
            sink,
            ..
        } = self;
        let Some(dev) = touch else {
            return true;
        };
        let fd = dev.file.as_raw_fd();
        loop {
            match evdev::read_events(fd, &mut events[..]) {
                Read::Events(n) => {
                    for ev in &events[..n] {
                        if dev.state.event(ev, &mut *sink) {
                            let snap = Self::snapshot(fd, dev.state.is_mt());
                            dev.state.resync(&snap, &mut *sink);
                        }
                    }
                }
                Read::WouldBlock => return true,
                Read::Gone => return false,
            }
        }
    }

    /// The touchscreen's state as the kernel holds it, for after dropped
    /// events. Where the device will not say, everything counts as lifted.
    fn snapshot(fd: RawFd, mt: bool) -> Snapshot {
        let mut snap = Snapshot::lifted();
        if mt {
            let ok = evdev::mt_slots(fd, evdev::ABS_MT_TRACKING_ID, &mut snap.ids).is_ok()
                && evdev::mt_slots(fd, evdev::ABS_MT_POSITION_X, &mut snap.x).is_ok()
                && evdev::mt_slots(fd, evdev::ABS_MT_POSITION_Y, &mut snap.y).is_ok();
            if !ok {
                snap.ids = [-1; MAX_SLOTS];
            }
            snap.slot = evdev::abs_info(fd, evdev::ABS_MT_SLOT).map_or(0, |a| a.value);
        } else {
            let mut keys = [0usize; evdev::KEY_WORDS];
            let down = evdev::key_state(fd, &mut keys).is_ok() && has(&keys, evdev::BTN_TOUCH);
            snap.ids[0] = if down { 0 } else { -1 };
            snap.x[0] = evdev::abs_info(fd, evdev::ABS_X).map_or(0, |a| a.value);
            snap.y[0] = evdev::abs_info(fd, evdev::ABS_Y).map_or(0, |a| a.value);
        }
        snap
    }

    fn drop_touch(&mut self) {
        if let Some(mut t) = self.touch.take() {
            t.state.device_gone(&mut self.sink);
            log::info!("input: touch {} gone", t.node);
        }
    }

    fn read_inotify(&mut self, now: u64) {
        let Some(at) = self.inotify_at else {
            return;
        };
        if self.fds[at].revents & libc::POLLIN == 0 {
            return;
        }
        let Some(fd) = self.inotify.as_ref().map(AsRawFd::as_raw_fd) else {
            return;
        };
        let mut buf = [0u8; 1024];
        loop {
            // SAFETY: the kernel writes at most buf.len() bytes into buf.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
            let Ok(n) = usize::try_from(n) else {
                return; // EAGAIN, or an error: either way nothing more now
            };
            if n == 0 {
                return;
            }
            let n = n.min(buf.len());
            hotplug::parse(&buf[..n], |notice| self.on_notice(notice, now));
        }
    }

    fn on_notice(&mut self, notice: Notice<'_>, now: u64) {
        if notice.wd == self.dev_wd {
            // devtmpfs removes /dev/input with its last node and makes a new
            // one for the next: watch that, and catch up on any node created
            // before the watch was in place.
            if notice.mask & libc::IN_CREATE != 0
                && notice.name == b"input"
                && self.watch_input_dir()
            {
                self.scan_existing(now);
            }
            return;
        }
        if notice.mask & libc::IN_IGNORED != 0 {
            if notice.wd == self.input_wd {
                self.input_wd = -1; // the directory is gone
            }
            return;
        }
        let Some(name) = NodeName::new(notice.name) else {
            return;
        };
        if !name.is_event() && self.touch_name != Some(name) {
            return;
        }
        if notice.mask & libc::IN_DELETE != 0 {
            self.node_gone(&name);
        } else {
            self.open_or_retry(name, now);
        }
    }

    fn node_gone(&mut self, name: &NodeName) {
        self.retries.retain(|r| r.node != *name);
        if self.touch.as_ref().is_some_and(|t| t.node == *name) {
            self.drop_touch();
        } else if let Some(i) = self.keyboards.iter().position(|k| k.node == *name) {
            self.drop_keyboard(i);
        }
    }

    fn scan_existing(&mut self, now: u64) {
        let Ok(dir) = std::fs::read_dir("/dev/input") else {
            return;
        };
        for entry in dir.flatten() {
            let file_name = entry.file_name();
            let Some(name) = NodeName::new(file_name.as_bytes()) else {
                continue;
            };
            if name.is_event() || self.touch_name == Some(name) {
                self.open_or_retry(name, now);
            }
        }
    }

    fn open_or_retry(&mut self, name: NodeName, now: u64) {
        if self.consider(&name, now) || self.retries.iter().any(|r| r.node == name) {
            return;
        }
        if self.retries.len() >= MAX_RETRIES {
            log::warn!("input: {name}: cannot open and no room to retry");
            return;
        }
        self.retries.push(Retry {
            node: name,
            next: now + OPEN_RETRY_MS,
            until: now + OPEN_RETRY_FOR_MS,
        });
    }

    fn has_node(&self, name: &NodeName) -> bool {
        self.touch.as_ref().is_some_and(|t| t.node == *name)
            || self.keyboards.iter().any(|k| k.node == *name)
    }

    /// Opens and sorts one node. False when it could not be opened yet, which
    /// is worth a retry.
    fn consider(&mut self, name: &NodeName, _now: u64) -> bool {
        if self.has_node(name) {
            return true;
        }
        let mut buf = [0u8; PATH_BUF];
        let Some(path) = name.path(&mut buf) else {
            return true;
        };
        match open_node(path) {
            Ok(file) => {
                self.adopt(file, *name);
                true
            }
            Err(e) => match e.kind() {
                io::ErrorKind::NotFound => true, // already gone: nothing to retry
                // A new node is root's for a moment, until udev gives it to
                // the input group: retried, and only then reported.
                io::ErrorKind::PermissionDenied => {
                    log::debug!("input: {name}: {e}");
                    false
                }
                _ => false,
            },
        }
    }

    /// The touchscreen, a keyboard, or neither (closed again).
    fn adopt(&mut self, file: File, name: NodeName) {
        let fd = file.as_raw_fd();
        let bluetooth = evdev::id(fd).is_ok_and(|id| id.bustype == evdev::BUS_BLUETOOTH);
        let mut abs = [0usize; evdev::ABS_WORDS];
        let mut key = [0usize; evdev::KEY_WORDS];
        let mut rel = [0usize; evdev::REL_WORDS];
        // A failed query leaves the bits clear: the device then has nothing.
        let _ = evdev::bits(fd, evdev::EV_ABS, &mut abs);
        let _ = evdev::bits(fd, evdev::EV_KEY, &mut key);
        let _ = evdev::bits(fd, evdev::EV_REL, &mut rel);
        let is_touch = match &self.touch_name {
            Some(wanted) => *wanted == name,
            // A Bluetooth device with absolute axes is a keyboard's touchpad
            // half, not the panel.
            None => !bluetooth && (has(&abs, evdev::ABS_MT_POSITION_X) || has(&abs, evdev::ABS_X)),
        };
        if self.touch.is_none() && is_touch {
            self.open_touch(file, name);
            return;
        }
        // Only Bluetooth HID devices are keyboards: the touchscreen's I2C
        // device reports keys too and must not be taken, let alone grabbed.
        // A device counts when it has keys this reader uses, a hat, or a
        // wheel: a keyboard's touchpad half is read for its wheel and click.
        if bluetooth
            && (keymap::useful_keys(&key)
                || has(&abs, evdev::ABS_HAT0X)
                || has(&abs, evdev::ABS_HAT0Y)
                || has(&rel, evdev::REL_WHEEL))
        {
            self.open_keyboard(file, name);
        }
    }

    fn open_touch(&mut self, file: File, name: NodeName) {
        let fd = file.as_raw_fd();
        let mut abs = [0usize; evdev::ABS_WORDS];
        let _ = evdev::bits(fd, evdev::EV_ABS, &mut abs);
        // Multitouch (protocol B) when the device has per-contact axes, else
        // single-touch ABS_X/ABS_Y with BTN_TOUCH.
        let mt = has(&abs, evdev::ABS_MT_POSITION_X);
        let (ax, ay) = if mt {
            (evdev::ABS_MT_POSITION_X, evdev::ABS_MT_POSITION_Y)
        } else {
            (evdev::ABS_X, evdev::ABS_Y)
        };
        let range = |axis| evdev::abs_info(fd, axis).map_or((0, 1), |a| (a.minimum, a.maximum));
        let (xr, yr) = (range(ax), range(ay));
        let nslots = if mt {
            evdev::abs_info(fd, evdev::ABS_MT_SLOT).map_or(MAX_SLOTS, |a| {
                a.maximum.saturating_add(1).clamp(1, MAX_SLOTS as i32) as usize
            })
        } else {
            1
        };
        let cal = Calibration::new(
            xr,
            yr,
            self.config.swap_xy,
            self.config.invert_x,
            self.config.invert_y,
        );
        let mut namebuf = [0u8; 80];
        log::info!(
            "input: touch {name} '{}' x {}..{} y {}..{} {} slots{}",
            evdev::name(fd, &mut namebuf),
            xr.0,
            xr.1,
            yr.0,
            yr.1,
            nslots,
            if mt { "" } else { " (single-touch)" }
        );
        self.touch = Some(TouchDev {
            file,
            node: name,
            state: Touch::new(cal, mt, nslots, self.shared.route()),
        });
    }

    fn open_keyboard(&mut self, file: File, name: NodeName) {
        if self.keyboards.len() >= MAX_KEYBOARDS {
            log::warn!("input: {name}: already {MAX_KEYBOARDS} keyboards; not read");
            return;
        }
        let fd = file.as_raw_fd();
        // Exclusive: no other handler sees these keys. Without it Magic SysRq,
        // enabled on the image, would let Alt+SysRq+B on a paired keyboard
        // reboot the head unit.
        if let Err(e) = evdev::grab(fd) {
            log::warn!("input: {name}: no exclusive grab ({e})");
        }
        let mut namebuf = [0u8; 80];
        log::info!("input: + {name} '{}'", evdev::name(fd, &mut namebuf));
        self.keyboards.push(KeyDev {
            file,
            node: name,
            kb: Keyboard::new(fd),
        });
    }

    /// The configured touchscreen, opened by its path: it may live outside
    /// /dev/input. Missing, it is waited for through hotplug if it is in
    /// there.
    fn open_configured_touch(&mut self) {
        let (Some(path), Some(name)) = (self.touch_path.as_deref(), self.touch_name) else {
            return;
        };
        match open_node(path) {
            Ok(file) => self.open_touch(file, name),
            Err(e) => log::warn!(
                "input: touch device {}: {e}; waiting for it",
                self.config.touch_device
            ),
        }
    }
}

/// The node's group may read and write it (udev's rule has been applied).
fn group_may_read(name: &NodeName) -> bool {
    let mut buf = [0u8; PATH_BUF];
    let Some(path) = name.path(&mut buf) else {
        return true;
    };
    // SAFETY: a zeroed stat filled in by the call, which reads only `path`.
    let mode = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::stat(path.as_ptr(), &mut st) != 0 {
            return true;
        }
        st.st_mode
    };
    group_rw(mode as u32)
}

fn group_rw(mode: u32) -> bool {
    mode & 0o060 == 0o060
}

fn open_node(path: &CStr) -> io::Result<File> {
    File::options()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(std::ffi::OsStr::from_bytes(path.to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::group_rw;

    #[test]
    fn only_a_node_its_group_may_use_blames_the_group() {
        assert!(group_rw(0o020_660)); // root:input 0660, udev's rule applied
        assert!(!group_rw(0o020_600)); // root's alone, as devtmpfs makes it
        assert!(!group_rw(0o020_640)); // read but not write: not udev's mode
    }
}
