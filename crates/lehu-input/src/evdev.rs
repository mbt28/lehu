//! The kernel's evdev interface through `libc`: the event layout, the codes
//! this crate acts on and the ioctls it needs. Nothing here interprets an
//! event; `touch` and `keys` do.

use std::io;
use std::mem::{size_of, size_of_val};
use std::os::fd::RawFd;

use crate::MAX_SLOTS;

// linux/input-event-codes.h
pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_REL: u16 = 0x02;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const SYN_DROPPED: u16 = 3;

pub const KEY_ESC: u16 = 1;
pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_ENTER: u16 = 28;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_SPACE: u16 = 57;
pub const KEY_CAPSLOCK: u16 = 58;
pub const KEY_KPENTER: u16 = 96;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_HOME: u16 = 102;
pub const KEY_UP: u16 = 103;
pub const KEY_LEFT: u16 = 105;
pub const KEY_RIGHT: u16 = 106;
pub const KEY_DOWN: u16 = 108;
pub const KEY_MUTE: u16 = 113;
pub const KEY_VOLUMEDOWN: u16 = 114;
pub const KEY_VOLUMEUP: u16 = 115;
pub const KEY_PAUSE: u16 = 119;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_COMPOSE: u16 = 127;
pub const KEY_MENU: u16 = 139;
pub const KEY_BACK: u16 = 158;
pub const KEY_NEXTSONG: u16 = 163;
pub const KEY_PLAYPAUSE: u16 = 164;
pub const KEY_PREVIOUSSONG: u16 = 165;
pub const KEY_HOMEPAGE: u16 = 172;
pub const KEY_PLAYCD: u16 = 200;
pub const KEY_PAUSECD: u16 = 201;
pub const KEY_PLAY: u16 = 207;
pub const KEY_SEARCH: u16 = 217;
pub const BTN_MIDDLE: u16 = 0x112;
pub const BTN_SOUTH: u16 = 0x130;
pub const BTN_EAST: u16 = 0x131;
pub const BTN_START: u16 = 0x13b;
pub const BTN_MODE: u16 = 0x13c;
pub const BTN_TOUCH: u16 = 0x14a;
pub const KEY_OK: u16 = 0x160;
pub const KEY_SELECT: u16 = 0x161;
pub const BTN_DPAD_UP: u16 = 0x220;
pub const BTN_DPAD_DOWN: u16 = 0x221;
pub const BTN_DPAD_LEFT: u16 = 0x222;
pub const BTN_DPAD_RIGHT: u16 = 0x223;
pub const KEY_VOICECOMMAND: u16 = 0x246;
pub const KEY_ASSISTANT: u16 = 0x247;
/// Key codes are below this.
pub const KEY_CNT: usize = 0x300;

pub const REL_WHEEL: u16 = 0x08;
pub const REL_CNT: usize = 0x10;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_HAT0X: u16 = 0x10;
pub const ABS_HAT0Y: u16 = 0x11;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
pub const ABS_CNT: usize = 0x40;

pub const BUS_BLUETOOTH: u16 = 0x05;

/// The kernel's `struct input_event`: the time is two native words, then
/// the type, the code and the value. The same on the 32-bit board and a
/// 64-bit host.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RawEvent {
    pub sec: usize,
    pub usec: usize,
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

#[cfg(test)]
impl RawEvent {
    pub const fn new(kind: u16, code: u16, value: i32) -> Self {
        Self {
            sec: 0,
            usec: 0,
            kind,
            code,
            value,
        }
    }
}

// Bit sets come from the kernel as `unsigned long` words.
const WORD_BITS: usize = usize::BITS as usize;
const fn words(bits: usize) -> usize {
    bits.div_ceil(WORD_BITS)
}
pub const KEY_WORDS: usize = words(KEY_CNT);
pub const REL_WORDS: usize = words(REL_CNT);
pub const ABS_WORDS: usize = words(ABS_CNT);

/// Is `bit` set in a kernel bit set? False beyond its end.
pub fn has(bits: &[usize], bit: u16) -> bool {
    let bit = usize::from(bit);
    bits.get(bit / WORD_BITS)
        .is_some_and(|w| (w >> (bit % WORD_BITS)) & 1 != 0)
}

// asm-generic ioctl encoding: direction 2 bits, size 14, type 8, number 8.
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const fn ioc(dir: u32, nr: u32, size: usize) -> libc::Ioctl {
    ((dir << 30) | ((size as u32) << 16) | ((b'E' as u32) << 8) | nr) as libc::Ioctl
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// EVIOCGBIT: the codes of event type `kind` the device reports.
pub fn bits(fd: RawFd, kind: u16, out: &mut [usize]) -> io::Result<()> {
    let req = ioc(IOC_READ, 0x20 + u32::from(kind), size_of_val(out));
    // SAFETY: EVIOCGBIT writes at most the size given, into `out`.
    check(unsafe { libc::ioctl(fd, req, out.as_mut_ptr()) })
}

/// EVIOCGKEY: the keys down right now.
pub fn key_state(fd: RawFd, out: &mut [usize; KEY_WORDS]) -> io::Result<()> {
    let req = ioc(IOC_READ, 0x18, size_of_val(out));
    // SAFETY: EVIOCGKEY writes at most the size given, into `out`.
    check(unsafe { libc::ioctl(fd, req, out.as_mut_ptr()) })
}

/// EVIOCGID: bus, vendor, product, version.
pub fn id(fd: RawFd) -> io::Result<libc::input_id> {
    let mut id = libc::input_id {
        bustype: 0,
        vendor: 0,
        product: 0,
        version: 0,
    };
    let req = ioc(IOC_READ, 0x02, size_of::<libc::input_id>());
    // SAFETY: EVIOCGID fills one struct input_id.
    check(unsafe { libc::ioctl(fd, req, &mut id as *mut libc::input_id) })?;
    Ok(id)
}

/// EVIOCGABS: the range and current value of one absolute axis.
pub fn abs_info(fd: RawFd, axis: u16) -> io::Result<libc::input_absinfo> {
    let mut info = libc::input_absinfo {
        value: 0,
        minimum: 0,
        maximum: 0,
        fuzz: 0,
        flat: 0,
        resolution: 0,
    };
    let req = ioc(
        IOC_READ,
        0x40 + u32::from(axis),
        size_of::<libc::input_absinfo>(),
    );
    // SAFETY: EVIOCGABS fills one struct input_absinfo.
    check(unsafe { libc::ioctl(fd, req, &mut info as *mut libc::input_absinfo) })?;
    Ok(info)
}

/// EVIOCGMTSLOTS: one multitouch value (`code`) for every slot. Slots the
/// device does not have are left as they were.
pub fn mt_slots(fd: RawFd, code: u16, out: &mut [i32; MAX_SLOTS]) -> io::Result<()> {
    #[repr(C)]
    struct Request {
        code: u32,
        values: [i32; MAX_SLOTS],
    }
    let mut req = Request {
        code: u32::from(code),
        values: *out,
    };
    let ioctl = ioc(IOC_READ, 0x0a, size_of::<Request>());
    // SAFETY: EVIOCGMTSLOTS reads the code and writes at most MAX_SLOTS values.
    check(unsafe { libc::ioctl(fd, ioctl, &mut req as *mut Request) })?;
    *out = req.values;
    Ok(())
}

/// EVIOCGNAME: the device's name, "?" when it has none.
pub fn name(fd: RawFd, buf: &mut [u8; 80]) -> &str {
    let req = ioc(IOC_READ, 0x06, buf.len());
    // SAFETY: EVIOCGNAME writes at most buf.len() bytes, into `buf`.
    if unsafe { libc::ioctl(fd, req, buf.as_mut_ptr()) } < 0 {
        return "?";
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    std::str::from_utf8(&buf[..end]).unwrap_or("?")
}

/// EVIOCGRAB: no other handler sees this device's events.
pub fn grab(fd: RawFd) -> io::Result<()> {
    let req = ioc(IOC_WRITE, 0x90, size_of::<libc::c_int>());
    // SAFETY: EVIOCGRAB takes the flag by value.
    check(unsafe { libc::ioctl(fd, req, 1 as libc::c_ulong) })
}

pub enum Read {
    /// This many events are in the buffer.
    Events(usize),
    /// Nothing more right now.
    WouldBlock,
    /// The device is gone.
    Gone,
}

/// One non-blocking read of whole events into `buf`.
pub fn read_events(fd: RawFd, buf: &mut [RawEvent]) -> Read {
    // SAFETY: the kernel writes whole events, at most the size of `buf`.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), size_of_val(buf)) };
    if n < 0 {
        return match io::Error::last_os_error().raw_os_error() {
            Some(libc::EAGAIN) | Some(libc::EINTR) => Read::WouldBlock,
            _ => Read::Gone,
        };
    }
    if n == 0 {
        return Read::Gone;
    }
    Read::Events(n as usize / size_of::<RawEvent>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_is_the_kernels_layout() {
        assert_eq!(size_of::<RawEvent>(), 2 * size_of::<usize>() + 8);
    }

    #[test]
    fn bit_sets() {
        let mut set = [0usize; KEY_WORDS];
        assert!(!has(&set, KEY_UP));
        set[usize::from(KEY_UP) / WORD_BITS] |= 1 << (usize::from(KEY_UP) % WORD_BITS);
        assert!(has(&set, KEY_UP));
        assert!(!has(&set, KEY_DOWN));
        assert!(!has(&set, 0x3ff)); // past the end
    }

    #[test]
    fn ioctl_numbers_match_the_header() {
        // EVIOCGRAB = _IOW('E', 0x90, int) = 0x40044590.
        assert_eq!(ioc(IOC_WRITE, 0x90, 4), 0x4004_4590);
        // EVIOCGID = _IOR('E', 0x02, struct input_id) = 0x80084502.
        assert_eq!(ioc(IOC_READ, 0x02, 8), 0x8008_4502);
    }
}
