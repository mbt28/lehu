//! The phone's USB side: finding it in sysfs, switching it to its CarPlay
//! configuration, and Apple's usbmux, a TCP multiplexer on two bulk
//! endpoints, through which lockdown and the carkit service are reached.
//! All through usbfs and sysfs with plain ioctls, as the trigger needs a
//! vendor control request that libusb refuses (it answered with a pipe
//! error where the raw ioctl succeeds); nothing here needs a USB library.
//!
//! Only the usbmux interface (1 in configuration 6) is claimed; the NCM
//! interfaces stay with the kernel's cdc_ncm, which brings up the network
//! link the CarPlay session itself runs over.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SYS_USB: &str = "/sys/bus/usb/devices";
const APPLE_VENDOR: &str = "05ac";

const MUX_MAGIC: u32 = 0xfeed_face;
const P_VERSION: u32 = 0;
const P_SETUP: u32 = 2;
const P_TCP: u32 = 6;
const TH_FIN: u8 = 0x01;
const TH_SYN: u8 = 0x02;
const TH_RST: u8 = 0x04;
const TH_ACK: u8 = 0x10;
const EP_OUT: u32 = 0x04;
const EP_IN: u32 = 0x85;
const TX_WINDOW: u32 = 131_072;
/// One TCP segment's payload; also the largest single bulk transfer usbfs
/// allows on this host controller.
const MAX_PAYLOAD: usize = 16_384;
/// A connection's unread bytes beyond this mean its reader is wedged; the
/// data cannot be dropped (it is a TCP stream), so the connection ends.
const MAX_QUEUED: usize = 4 << 20;
/// A mux packet longer than this is a framing error.
const MAX_MUX_PACKET: usize = 1 << 20;

// ── usbfs ─────────────────────────────────────────────────────────────────

#[repr(C)]
struct CtrlTransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
    data: *mut libc::c_void,
}

#[repr(C)]
struct BulkTransfer {
    ep: u32,
    len: u32,
    timeout: u32,
    data: *mut libc::c_void,
}

/// `struct usbdevfs_urb`, without the isochronous frames.
#[repr(C)]
struct Urb {
    kind: u8,
    endpoint: u8,
    status: libc::c_int,
    flags: libc::c_uint,
    buffer: *mut libc::c_void,
    buffer_length: libc::c_int,
    actual_length: libc::c_int,
    start_frame: libc::c_int,
    number_of_packets: libc::c_int,
    error_count: libc::c_int,
    signr: libc::c_uint,
    usercontext: *mut libc::c_void,
}

const USBDEVFS_URB_TYPE_BULK: u8 = 3;

const fn ioc(dir: libc::c_ulong, nr: libc::c_ulong, size: usize) -> libc::c_ulong {
    (dir << 30) | ((size as libc::c_ulong) << 16) | ((b'U' as libc::c_ulong) << 8) | nr
}
const IOC_WRITE: libc::c_ulong = 1;
const IOC_READ: libc::c_ulong = 2;
const IOC_RW: libc::c_ulong = 3;
const USBDEVFS_CONTROL: libc::c_ulong = ioc(IOC_RW, 0, std::mem::size_of::<CtrlTransfer>());
const USBDEVFS_BULK: libc::c_ulong = ioc(IOC_RW, 2, std::mem::size_of::<BulkTransfer>());
const USBDEVFS_SETCONFIGURATION: libc::c_ulong = ioc(IOC_READ, 5, 4);
const USBDEVFS_SUBMITURB: libc::c_ulong = ioc(IOC_READ, 10, std::mem::size_of::<Urb>());
const USBDEVFS_DISCARDURB: libc::c_ulong = ioc(0, 11, 0);
const USBDEVFS_REAPURBNDELAY: libc::c_ulong =
    ioc(IOC_WRITE, 13, std::mem::size_of::<*mut libc::c_void>());
const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = ioc(IOC_READ, 15, 4);
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = ioc(IOC_READ, 16, 4);

fn usb_control(
    fd: &OwnedFd,
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    data: &mut [u8],
) -> io::Result<()> {
    let mut ct = CtrlTransfer {
        request_type,
        request,
        value,
        index,
        length: data.len() as u16,
        timeout: 3000,
        data: data.as_mut_ptr().cast(),
    };
    // SAFETY: the struct and the buffer it points to outlive the call.
    if unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_CONTROL as _, &mut ct) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// A bulk transfer; bytes moved, or the error.
fn usb_bulk(fd: &OwnedFd, ep: u32, data: &mut [u8], timeout_ms: u32) -> io::Result<usize> {
    let mut bt = BulkTransfer {
        ep,
        len: data.len() as u32,
        timeout: timeout_ms,
        data: data.as_mut_ptr().cast(),
    };
    // SAFETY: the struct and the buffer it points to outlive the call.
    let n = unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_BULK as _, &mut bt) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

fn usb_int_ioctl(fd: &OwnedFd, req: libc::c_ulong, value: u32) -> io::Result<()> {
    let mut v = value;
    // SAFETY: the ioctl reads one u32 through the pointer.
    if unsafe { libc::ioctl(fd.as_raw_fd(), req as _, &mut v) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Where the stream goes on after a header that cannot be one.
#[derive(Debug, PartialEq, Eq)]
enum Resync {
    /// A header starts at this offset.
    At(usize),
    /// Nothing yet: the bytes before this offset go, the rest waits for
    /// more.
    Wait(usize),
}

/// This host controller shares one receive endpoint among the phone's
/// bulk-IN endpoints and now and then hands a transfer meant for another
/// interface to ours: a CDC-NCM block of the phone's network link. That
/// block is skipped whole, by the length in its own header; anything else
/// up to the next mux header (its magic, and a length that can be one).
/// The mux bytes the swap cost are sent again by the phone's TCP.
fn resync(rx: &[u8], off: usize) -> Resync {
    let rest = &rx[off..];
    if rest.starts_with(b"NCMH") {
        if rest.len() < 12 {
            return Resync::Wait(off);
        }
        let block = usize::from(u16::from_le_bytes([rest[8], rest[9]]));
        if block >= 12 {
            return if rest.len() >= block {
                Resync::At(off + block)
            } else {
                Resync::Wait(off)
            };
        }
    }
    let magic = MUX_MAGIC.to_be_bytes();
    for at in off + 1..rx.len().saturating_sub(11) {
        if rx[at + 8..at + 12] == magic {
            let len = u32::from_be_bytes([rx[at + 4], rx[at + 5], rx[at + 6], rx[at + 7]]);
            if (16..=MAX_MUX_PACKET as u32).contains(&len) {
                return Resync::At(at);
            }
        }
    }
    // A header may start in the last 11 bytes.
    Resync::Wait(rx.len().saturating_sub(11).max(off))
}

/// The bulk IN endpoint read with asynchronous URBs, several queued at
/// once. A synchronous `USBDEVFS_BULK` that times out drops what it had
/// received (usbfs copies data out only on success), and the mux has no
/// retransmission. With one transfer at a time the bus is not read at all
/// between a completion and the next submission: a reader stalled there
/// (a busy CPU, a page fault while the SD card is busy) left the phone
/// with nowhere to send, and the stream lost bytes. Queued transfers keep
/// the host reading into the kernel's buffers meanwhile; they complete,
/// and are reaped, in the order they were submitted. `poll` bounds the
/// wait.
struct InUrb {
    urb: Box<Urb>,
    buf: Vec<u8>,
    in_flight: bool,
}

/// Transfers queued on the IN endpoint, 16 KB each.
const IN_URBS: usize = 4;

struct InQueue {
    urbs: Vec<InUrb>,
}

impl InQueue {
    fn new(ep: u8, len: usize) -> Self {
        let urbs = (0..IN_URBS)
            .map(|_| {
                let mut buf = vec![0u8; len];
                let urb = Box::new(Urb {
                    kind: USBDEVFS_URB_TYPE_BULK,
                    endpoint: ep,
                    status: 0,
                    flags: 0,
                    buffer: buf.as_mut_ptr().cast(),
                    buffer_length: len as libc::c_int,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets: 0,
                    error_count: 0,
                    signr: 0,
                    usercontext: std::ptr::null_mut(),
                });
                InUrb {
                    urb,
                    buf,
                    in_flight: false,
                }
            })
            .collect();
        Self { urbs }
    }

    /// The next completed transfer: its index and the bytes it holds in
    /// `buf(index)`; `None` when nothing came within `timeout_ms` (the
    /// transfers stay queued, nothing is lost). Every idle transfer is
    /// queued again first, so the one just read is back in the queue.
    fn read(&mut self, fd: &OwnedFd, timeout_ms: i32) -> io::Result<Option<(usize, usize)>> {
        for u in self.urbs.iter_mut().filter(|u| !u.in_flight) {
            u.urb.status = 0;
            u.urb.actual_length = 0;
            // SAFETY: the URB and the buffer it points to are heap
            // allocations that outlive the transfer: reaped here, or
            // discarded in `cancel` (usbfs copies into them only on a reap).
            if unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_SUBMITURB as _, &mut *u.urb) } < 0 {
                return Err(io::Error::last_os_error());
            }
            u.in_flight = true;
        }
        let mut done: *mut Urb = std::ptr::null_mut();
        // SAFETY: the ioctl writes one pointer, to the URB it completed.
        let reaped =
            unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_REAPURBNDELAY as _, &mut done) } == 0;
        if !reaped {
            let e = io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EAGAIN) {
                return Err(e);
            }
            let mut pfd = libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLOUT,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the call.
            let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if n < 0 {
                let e = io::Error::last_os_error();
                return if e.kind() == io::ErrorKind::Interrupted {
                    Ok(None)
                } else {
                    Err(e)
                };
            }
            if pfd.revents & (libc::POLLHUP | libc::POLLERR) != 0 {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            if pfd.revents & libc::POLLOUT == 0 {
                return Ok(None);
            }
            // SAFETY: as above.
            if unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_REAPURBNDELAY as _, &mut done) } < 0 {
                let e = io::Error::last_os_error();
                return if e.raw_os_error() == Some(libc::EAGAIN) {
                    Ok(None)
                } else {
                    Err(e)
                };
            }
        }
        let Some(index) = self
            .urbs
            .iter()
            .position(|u| std::ptr::eq(&*u.urb as *const Urb, done.cast_const()))
        else {
            return Err(io::Error::other(
                "usbfs returned a transfer that is not ours",
            ));
        };
        let u = &mut self.urbs[index];
        u.in_flight = false;
        if u.urb.status != 0 {
            return Err(io::Error::from_raw_os_error(-u.urb.status));
        }
        Ok(Some((index, u.urb.actual_length.max(0) as usize)))
    }

    fn buf(&self, index: usize) -> &[u8] {
        &self.urbs[index].buf
    }

    /// Cancel the queued transfers and take them back.
    fn cancel(&mut self, fd: &OwnedFd) {
        for u in self.urbs.iter_mut().filter(|u| u.in_flight) {
            // SAFETY: the URB was submitted on this fd; the ioctl only takes
            // the pointer.
            unsafe {
                libc::ioctl(fd.as_raw_fd(), USBDEVFS_DISCARDURB as _, &mut *u.urb);
            }
        }
        let mut done: *mut Urb = std::ptr::null_mut();
        for _ in 0..100 {
            if self.urbs.iter().all(|u| !u.in_flight) {
                break;
            }
            // SAFETY: the reap writes the pointer of a URB it hands back.
            if unsafe { libc::ioctl(fd.as_raw_fd(), USBDEVFS_REAPURBNDELAY as _, &mut done) } == 0 {
                for u in &mut self.urbs {
                    if std::ptr::eq(&*u.urb as *const Urb, done.cast_const()) {
                        u.in_flight = false;
                    }
                }
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

// ── sysfs ─────────────────────────────────────────────────────────────────

fn attr(dir: &Path, name: &str) -> Option<String> {
    fs::read_to_string(dir.join(name))
        .ok()
        .map(|s| s.trim().to_string())
}

/// The plugged phone as sysfs describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Phone {
    /// The USB serial, which is the phone's UDID without its dash.
    pub serial: String,
    /// The sysfs name, such as "1-1".
    pub sysname: String,
    pub bus: u8,
    pub address: u8,
}

impl Phone {
    pub fn sysdir(&self) -> PathBuf {
        Path::new(SYS_USB).join(&self.sysname)
    }

    pub fn node(&self) -> String {
        format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.address)
    }

    pub fn open(&self) -> io::Result<OwnedFd> {
        Ok(fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.node())?
            .into())
    }

    fn configurations(&self) -> u32 {
        attr(&self.sysdir(), "bNumConfigurations")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    pub fn configuration(&self) -> String {
        attr(&self.sysdir(), "bConfigurationValue").unwrap_or_default()
    }
}

/// The serial of the device at sysfs name `sysname`, when it is Apple's.
pub fn serial_of(sysname: &str) -> Option<String> {
    let dir = Path::new(SYS_USB).join(sysname);
    if attr(&dir, "idVendor")? != APPLE_VENDOR {
        return None;
    }
    attr(&dir, "serial").filter(|s| !s.is_empty())
}

/// The Apple device with this serial, wherever it is on the bus now: the
/// configuration switch re-enumerates it, so its address moves.
pub fn find_by_serial(serial: &str) -> Option<Phone> {
    for entry in fs::read_dir(SYS_USB).ok()?.flatten() {
        let dir = entry.path();
        if attr(&dir, "idVendor").as_deref() != Some(APPLE_VENDOR) {
            continue;
        }
        if attr(&dir, "serial").as_deref() != Some(serial) {
            continue;
        }
        let sysname = entry.file_name().to_string_lossy().into_owned();
        return Some(Phone {
            serial: serial.to_string(),
            sysname,
            bus: attr(&dir, "busnum")?.parse().ok()?,
            address: attr(&dir, "devnum")?.parse().ok()?,
        });
    }
    None
}

/// Bring the phone to configuration 6, its CarPlay mode: the hidden
/// configurations are revealed with the vendor request 0xc0/0x52 (the
/// phone re-enumerates with six), then 6 is selected. The ioctl works
/// without privileges only while no driver holds an interface; the kernel
/// drivers of configuration 4 (ipheth, cdc_ncm) make it busy, so the
/// sysfs write, which detaches them, is the fallback; it needs
/// `cap_dac_override` or root. Both are retried: the freshly revealed
/// device is briefly busy. `stop` ends the wait early.
pub fn configure_carplay(serial: &str, stop: &AtomicBool) -> Result<Phone, String> {
    let mut phone =
        find_by_serial(serial).ok_or_else(|| format!("iPhone {serial} not on the bus"))?;
    if phone.configurations() < 6 {
        let fd = phone
            .open()
            .map_err(|e| format!("open {}: {e}", phone.node()))?;
        let mut reply = [0u8; 1];
        if let Err(e) = usb_control(&fd, 0xc0, 0x52, 0, 4, &mut reply) {
            log::debug!("usbmux: the reveal request answered {e}");
        }
        drop(fd);
        let mut revealed = false;
        for _ in 0..30 {
            if stop.load(Ordering::Relaxed) {
                return Err("stopped".into());
            }
            thread::sleep(Duration::from_millis(200));
            if let Some(p) = find_by_serial(serial) {
                phone = p;
                if phone.configurations() >= 6 {
                    revealed = true;
                    break;
                }
            }
        }
        if !revealed {
            return Err(format!(
                "the iPhone kept {} configurations",
                phone.configurations()
            ));
        }
    }
    // Let the re-enumeration settle before switching.
    thread::sleep(Duration::from_millis(500));
    for attempt in 0..20 {
        if stop.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        if let Some(p) = find_by_serial(serial) {
            phone = p;
        }
        if phone.configuration() == "6" {
            return Ok(phone);
        }
        let ioctl = phone
            .open()
            .and_then(|fd| usb_int_ioctl(&fd, USBDEVFS_SETCONFIGURATION, 6));
        let mut sysfs = Ok(());
        if ioctl.is_err() {
            sysfs = fs::write(phone.sysdir().join("bConfigurationValue"), "6");
        }
        log::debug!(
            "usbmux: configuration 6, attempt {attempt}: now {}, ioctl {:?}, sysfs {:?}",
            phone.configuration(),
            ioctl.err(),
            sysfs.err()
        );
        thread::sleep(Duration::from_millis(400));
    }
    Err(format!(
        "could not select configuration 6 (still {}): LEHU needs cap_dac_override for the sysfs write",
        phone.configuration()
    ))
}

// ── A TCP connection over the mux ─────────────────────────────────────────

struct ConnState {
    tx_seq: u32,
    tx_ack: u32,
    connected: bool,
    closed: bool,
    queue: VecDeque<Vec<u8>>,
    queued: usize,
    /// A segment past a gap was refused and the gap is not filled yet:
    /// said once.
    gap: bool,
}

struct Conn {
    host: Arc<HostInner>,
    sport: u16,
    dport: u16,
    state: Mutex<ConnState>,
    ready: Condvar,
}

impl Conn {
    fn lock(&self) -> MutexGuard<'_, ConnState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Send one segment. The sequence numbers are sampled, the packet
    /// sent and the payload accounted for under one lock: two threads
    /// (the reader acknowledging, a writer sending) must never build two
    /// segments with the same sequence number, which on a TLS stream was
    /// silent corruption.
    fn segment(&self, flags: u8, payload: &[u8]) -> bool {
        let mut st = self.lock();
        let mut th = Vec::with_capacity(20 + payload.len());
        th.extend_from_slice(&self.sport.to_be_bytes());
        th.extend_from_slice(&self.dport.to_be_bytes());
        th.extend_from_slice(&st.tx_seq.to_be_bytes());
        th.extend_from_slice(&st.tx_ack.to_be_bytes());
        th.push(0x50);
        th.push(flags);
        th.extend_from_slice(&((TX_WINDOW >> 8) as u16).to_be_bytes());
        th.extend_from_slice(&[0, 0, 0, 0]);
        th.extend_from_slice(payload);
        // Sequence space is consumed only by a segment the phone received;
        // advancing after a failed write makes every later one look out of
        // order.
        if !self.host.mux_send(P_TCP, &th) {
            log::warn!(
                "usbmux: port {} could not send {} bytes",
                self.sport,
                payload.len()
            );
            return false;
        }
        st.tx_seq = st.tx_seq.wrapping_add(payload.len() as u32);
        true
    }

    fn on_packet(&self, flags: u8, seq: u32, payload: &[u8]) {
        if flags & TH_SYN != 0 && flags & TH_ACK != 0 {
            {
                let mut st = self.lock();
                st.tx_seq = st.tx_seq.wrapping_add(1);
                st.tx_ack = seq.wrapping_add(1);
                st.connected = true;
            }
            self.segment(TH_ACK, &[]);
            self.ready.notify_all();
            return;
        }
        if flags & TH_RST != 0 {
            self.mark_closed();
            return;
        }
        if !payload.is_empty() {
            let (taken, overflow) = self.take(seq, payload);
            if taken {
                self.ready.notify_all();
            }
            // Always acknowledged: the acknowledgement names the next byte
            // wanted, so after a gap the phone sends it again.
            self.segment(TH_ACK, &[]);
            if overflow {
                log::warn!("usbmux: port {} reader stalled, closing", self.sport);
                self.mark_closed();
            }
        }
        if flags & TH_FIN != 0 {
            {
                let mut st = self.lock();
                st.tx_ack = st.tx_ack.wrapping_add(1);
            }
            self.segment(TH_ACK, &[]);
            self.mark_closed();
        }
    }

    /// The stream takes only the bytes it expects next. A segment past a
    /// gap (one before it was lost on the way) is refused until the phone
    /// sends the missing one again; one already had, or the part of it
    /// already had, is dropped. Appended out of order they were silent
    /// corruption: the TLS on the link failed seconds later. Whether
    /// anything was taken, and whether the queue overflowed.
    fn take(&self, seq: u32, payload: &[u8]) -> (bool, bool) {
        let mut st = self.lock();
        let ahead = seq.wrapping_sub(st.tx_ack) as i32;
        if ahead > 0 {
            if !st.gap {
                st.gap = true;
                log::warn!(
                    "usbmux: port {}: {ahead} bytes missing before a segment; asking for them again",
                    self.sport
                );
            }
            return (false, false);
        }
        let had = ahead.unsigned_abs() as usize;
        if had >= payload.len() {
            return (false, false);
        }
        let fresh = &payload[had..];
        if st.gap {
            st.gap = false;
            log::info!("usbmux: port {}: the missing bytes came again", self.sport);
        }
        st.tx_ack = st.tx_ack.wrapping_add(fresh.len() as u32);
        st.queued += fresh.len();
        st.queue.push_back(fresh.to_vec());
        (true, st.queued > MAX_QUEUED)
    }

    fn mark_closed(&self) {
        self.lock().closed = true;
        self.ready.notify_all();
    }

    fn wait_connected(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut st = self.lock();
        while !st.connected && !st.closed {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            st = self
                .ready
                .wait_timeout(st, deadline - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        st.connected && !st.closed
    }
}

/// One mux connection as a byte stream. Reads wait up to the timeout set
/// and then return `WouldBlock`, so a protocol on top can run its timers;
/// the end of the connection reads as end of file.
pub struct MuxStream {
    conn: Arc<Conn>,
    chunk: Vec<u8>,
    pos: usize,
    timeout: Duration,
}

impl MuxStream {
    pub fn set_read_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    pub fn is_closed(&self) -> bool {
        self.conn.lock().closed
    }
}

impl Read for MuxStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.chunk.len() {
            let deadline = Instant::now() + self.timeout;
            let mut st = self.conn.lock();
            loop {
                if let Some(c) = st.queue.pop_front() {
                    st.queued -= c.len();
                    self.chunk = c;
                    self.pos = 0;
                    break;
                }
                if st.closed {
                    return Ok(0);
                }
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "no data yet"));
                }
                st = self
                    .conn
                    .ready
                    .wait_timeout(st, deadline - now)
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
        let n = buf.len().min(self.chunk.len() - self.pos);
        buf[..n].copy_from_slice(&self.chunk[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for MuxStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.conn.lock().closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "mux connection closed",
            ));
        }
        for chunk in buf.chunks(MAX_PAYLOAD) {
            if !self.conn.segment(TH_ACK, chunk) {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "usb write failed",
                ));
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        let closed = self.conn.lock().closed;
        if !closed {
            self.conn.segment(TH_FIN | TH_ACK, &[]);
        }
        self.conn.host.drop_conn(self.conn.sport);
    }
}

// ── The host ──────────────────────────────────────────────────────────────

struct HostInner {
    fd: OwnedFd,
    run: AtomicBool,
    /// Serialises writes and owns the mux transmit sequence.
    tx: Mutex<u16>,
    /// The phone's last transmit sequence, echoed in our packets.
    rx: AtomicU16,
    conns: Mutex<Vec<Arc<Conn>>>,
}

/// Source ports are never reused in the process's life: the retry loop
/// rebuilds the host every few seconds and the phone still holds state for
/// the previous connection on a port, answering a new SYN there with RST.
static NEXT_SPORT: AtomicU16 = AtomicU16::new(1);

impl HostInner {
    /// Frame and send one mux packet: `[proto][16 + len][magic][tx][rx]`.
    /// The sequence is consumed only by a packet the phone received; a
    /// failed write must not advance it, or the phone reports a duplicate
    /// and the link is out of step for good.
    ///
    /// The write is retried: this host controller returns `ESHUTDOWN` on a
    /// transient three-strikes error with the phone perfectly healthy, and
    /// the next transfer succeeds. The mux has no retransmission, so a
    /// packet lost here stalls the connection for good. A write that
    /// reports success but delivers nothing (the sunxi MUSB with its DMA
    /// on) cannot be caught here at all: the board boots with
    /// `musb_hdrc.use_dma=0`.
    fn mux_send(&self, proto: u32, payload: &[u8]) -> bool {
        let mut tx = self.tx.lock().unwrap_or_else(|e| e.into_inner());
        if !self.run.load(Ordering::Relaxed) {
            return false;
        }
        let mut pkt = Vec::with_capacity(16 + payload.len());
        pkt.extend_from_slice(&proto.to_be_bytes());
        pkt.extend_from_slice(&((16 + payload.len()) as u32).to_be_bytes());
        pkt.extend_from_slice(&MUX_MAGIC.to_be_bytes());
        pkt.extend_from_slice(&tx.to_be_bytes());
        pkt.extend_from_slice(&self.rx.load(Ordering::Relaxed).to_be_bytes());
        pkt.extend_from_slice(payload);
        for attempt in 1..=3 {
            match usb_bulk(&self.fd, EP_OUT, &mut pkt, 2000) {
                Ok(n) if n == pkt.len() => {
                    log::trace!(
                        "usbmux: tx proto {proto} {n} bytes, seq {} ack {}",
                        *tx,
                        self.rx.load(Ordering::Relaxed)
                    );
                    *tx = tx.wrapping_add(1);
                    return true;
                }
                Ok(n) => {
                    // Half a packet is on the phone: its framing is lost,
                    // and resending would compound it.
                    log::error!("usbmux: short write, {n} of {} bytes", pkt.len());
                    return false;
                }
                Err(e) => log::warn!(
                    "usbmux: write of {} bytes failed ({e}), attempt {attempt} of 3",
                    pkt.len()
                ),
            }
        }
        false
    }

    fn find(&self, sport: u16) -> Option<Arc<Conn>> {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.sport == sport)
            .cloned()
    }

    fn drop_conn(&self, sport: u16) {
        self.conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|c| c.sport != sport);
    }

    fn reader(&self) {
        let mut urbs = InQueue::new(EP_IN as u8, MAX_PAYLOAD);
        let mut rx: Vec<u8> = Vec::with_capacity(2 * MAX_PAYLOAD);
        // The last packet taken whole (protocol, length) and the bytes read
        // since: what a resync's line shows, to tell a lost header from
        // lost payload.
        let mut last = (0u32, 0usize);
        let mut since_last = 0usize;
        // Bytes skipped so far while looking for the next header.
        let mut skipped: Option<usize> = None;
        while self.run.load(Ordering::Relaxed) {
            let (index, n) = match urbs.read(&self.fd, 250) {
                Ok(Some(done)) => done,
                Ok(None) => continue,
                Err(e) => {
                    if self.run.load(Ordering::Relaxed) {
                        log::warn!("usbmux: the reader ended: {e}");
                    }
                    break;
                }
            };
            rx.extend_from_slice(&urbs.buf(index)[..n]);
            since_last += n;
            let mut off = 0;
            while rx.len() - off >= 8 {
                let proto = u32::from_be_bytes([rx[off], rx[off + 1], rx[off + 2], rx[off + 3]]);
                let len = u32::from_be_bytes([rx[off + 4], rx[off + 5], rx[off + 6], rx[off + 7]])
                    as usize;
                // A corrupt length would otherwise leave the loop waiting
                // for bytes that never come while the buffer grows.
                if !(8..=MAX_MUX_PACKET).contains(&len) {
                    if skipped.is_none() {
                        let head: String = rx[off..rx.len().min(off + 48)]
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect();
                        log::warn!(
                            "usbmux: implausible packet length {len}, resyncing (after protocol {} of {} bytes, {since_last} bytes read since, {} buffered; here: {head})",
                            last.0,
                            last.1,
                            rx.len() - off
                        );
                    }
                    match resync(&rx, off) {
                        Resync::At(next) => {
                            let total = skipped.take().unwrap_or(0) + (next - off);
                            log::info!("usbmux: in step again, {total} bytes skipped");
                            off = next;
                            continue;
                        }
                        Resync::Wait(keep) => {
                            skipped = Some(skipped.unwrap_or(0) + (keep - off));
                            off = keep;
                            break;
                        }
                    }
                }
                if rx.len() - off < len {
                    break;
                }
                let pkt = &rx[off..off + len];
                last = (proto, len);
                since_last = rx.len() - off - len;
                if len >= 16 {
                    self.rx
                        .store(u16::from_be_bytes([pkt[12], pkt[13]]), Ordering::Relaxed);
                }
                if proto == P_TCP && len >= 36 {
                    let dport = u16::from_be_bytes([pkt[18], pkt[19]]);
                    let seq = u32::from_be_bytes([pkt[20], pkt[21], pkt[22], pkt[23]]);
                    let flags = pkt[29];
                    log::trace!(
                        "usbmux: rx tcp {}->{dport} flags {flags:#04x} {} bytes",
                        u16::from_be_bytes([pkt[16], pkt[17]]),
                        len - 36
                    );
                    match self.find(dport) {
                        Some(conn) => conn.on_packet(flags, seq, &pkt[36..]),
                        None => log::debug!("usbmux: a segment for unknown port {dport}"),
                    }
                } else if log::log_enabled!(log::Level::Debug) {
                    // The phone's own channel carries plain-text diagnostics
                    // ("Invalid connection"): worth a line when it fails.
                    let text: String = pkt[16.min(len)..]
                        .iter()
                        .take(96)
                        .map(|&b| {
                            if b.is_ascii_graphic() || b == b' ' {
                                b as char
                            } else {
                                '.'
                            }
                        })
                        .collect();
                    log::debug!("usbmux: protocol {proto}, {len} bytes: {text}");
                }
                off += len;
            }
            if off > 0 {
                rx.drain(..off);
            }
        }
        urbs.cancel(&self.fd);
        // The reader died with the host running: the cable was pulled.
        // Every connection fails now, so blocked readers see end of file
        // and the session unwinds; there is no phone left to tell.
        if self.run.load(Ordering::Relaxed) {
            let conns = std::mem::take(&mut *self.conns.lock().unwrap_or_else(|e| e.into_inner()));
            for c in conns {
                c.mark_closed();
            }
        }
    }
}

pub struct MuxHost {
    inner: Arc<HostInner>,
    reader: Option<JoinHandle<()>>,
}

impl MuxHost {
    /// Claim the usbmux interface of the phone (in configuration 6) and
    /// open the mux: the version handshake and the setup packet.
    /// `realtime`: the reader asks for SCHED_FIFO above the audio threads
    /// (refused without the rtprio limit; it then runs normally).
    pub fn open(phone: &Phone, realtime: bool) -> io::Result<Self> {
        let fd = phone.open()?;
        // The kernel may hold the interface briefly after the switch.
        let mut claimed = false;
        for _ in 0..12 {
            if usb_int_ioctl(&fd, USBDEVFS_CLAIMINTERFACE, 1).is_ok() {
                claimed = true;
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        if !claimed {
            return Err(io::Error::other("cannot claim the usbmux interface"));
        }
        // Version: [proto 0][len 20][version 2][0][0], no magic, no sequence.
        let mut version = Vec::new();
        for v in [P_VERSION, 20, 2, 0, 0] {
            version.extend_from_slice(&v.to_be_bytes());
        }
        usb_bulk(&fd, EP_OUT, &mut version, 2000)?;
        // A healthy phone answers at once. One left in configuration 6 by
        // an earlier process, or one whose endpoints the controller
        // stalled, answers nothing at all, ever: the caller resets its
        // port then, which is what brings it back.
        let mut reply = [0u8; 4096];
        match usb_bulk(&fd, EP_IN, &mut reply, 2000) {
            Ok(n) => log::debug!("usbmux: version reply, {n} bytes"),
            Err(e) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("no version reply ({e})"),
                ));
            }
        }
        let inner = Arc::new(HostInner {
            fd,
            run: AtomicBool::new(true),
            // The mux sequences start at 0 per host: the phone resets its
            // own when it re-enumerates, and says so when we do not.
            tx: Mutex::new(0),
            rx: AtomicU16::new(0),
            conns: Mutex::new(Vec::new()),
        });
        let worker = Arc::clone(&inner);
        let reader = thread::Builder::new()
            .name("cp-mux".into())
            .spawn(move || {
                if realtime {
                    let param = libc::sched_param { sched_priority: 20 };
                    // SAFETY: the param struct is live for the call; a
                    // refusal leaves the thread as it is.
                    unsafe {
                        libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
                    }
                }
                worker.reader();
            })?;
        inner.mux_send(P_SETUP, &[0x07]);
        Ok(Self {
            inner,
            reader: Some(reader),
        })
    }

    /// Open a connection to a port on the phone (lockdown's 62078, a
    /// service's port).
    pub fn connect(&self, dport: u16) -> io::Result<MuxStream> {
        let mut sport = NEXT_SPORT.fetch_add(1, Ordering::Relaxed);
        if sport == 0 {
            sport = NEXT_SPORT.fetch_add(1, Ordering::Relaxed);
        }
        let conn = Arc::new(Conn {
            host: Arc::clone(&self.inner),
            sport,
            dport,
            state: Mutex::new(ConnState {
                tx_seq: 0,
                tx_ack: 0,
                connected: false,
                closed: false,
                queue: VecDeque::new(),
                queued: 0,
                gap: false,
            }),
            ready: Condvar::new(),
        });
        self.inner
            .conns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&conn));
        conn.segment(TH_SYN, &[]);
        if !conn.wait_connected(Duration::from_secs(5)) {
            self.inner.drop_conn(sport);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer from port {dport}"),
            ));
        }
        log::debug!("usbmux: port {sport} connected to {dport}");
        Ok(MuxStream {
            conn,
            chunk: Vec::new(),
            pos: 0,
            timeout: Duration::from_secs(5),
        })
    }

    /// Whether the reader thread is still serving the bulk endpoint.
    pub fn alive(&self) -> bool {
        self.reader.as_ref().is_some_and(|r| !r.is_finished())
    }

    /// End the mux: the reader stops, every connection is reset on the
    /// phone (marking it closed only on our side leaves the phone's half
    /// open; the retry loop rebuilds the host every few seconds and the
    /// leaked halves made the phone stop answering SYNs after minutes),
    /// and the interface is released.
    pub fn close(mut self) {
        let conns =
            std::mem::take(&mut *self.inner.conns.lock().unwrap_or_else(|e| e.into_inner()));
        for c in &conns {
            c.segment(TH_RST | TH_ACK, &[]);
            c.mark_closed();
        }
        self.inner.run.store(false, Ordering::Relaxed);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let _ = usb_int_ioctl(&self.inner.fd, USBDEVFS_RELEASEINTERFACE, 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // From linux/usbdevice_fs.h on this host's word size.
        let ptr = std::mem::size_of::<usize>();
        assert_eq!(
            std::mem::size_of::<CtrlTransfer>(),
            8 + 4 + if ptr == 8 { 4 + 8 } else { 4 }
        );
        assert_eq!(USBDEVFS_CLAIMINTERFACE, 0x8004_550f);
        assert_eq!(USBDEVFS_RELEASEINTERFACE, 0x8004_5510);
        assert_eq!(USBDEVFS_SETCONFIGURATION, 0x8004_5505);
        assert_eq!(USBDEVFS_BULK & 0xffff, 0x5502);
        // struct usbdevfs_urb: 44 bytes on 32-bit ARM, 56 on 64-bit.
        let urb = if ptr == 8 { 56 } else { 44 };
        assert_eq!(std::mem::size_of::<Urb>(), urb);
        assert_eq!(
            USBDEVFS_SUBMITURB,
            0x8000_550a | ((urb as libc::c_ulong) << 16)
        );
        assert_eq!(USBDEVFS_DISCARDURB, 0x550b);
        assert_eq!(
            USBDEVFS_REAPURBNDELAY,
            0x4000_550d | ((ptr as libc::c_ulong) << 16)
        );
    }

    /// A connection on a host that sends nothing (not running): what the
    /// stream takes is all there is to see.
    fn conn(next: u32) -> Conn {
        let fd = OwnedFd::from(fs::File::open("/dev/null").unwrap());
        Conn {
            host: Arc::new(HostInner {
                fd,
                run: AtomicBool::new(false),
                tx: Mutex::new(0),
                rx: AtomicU16::new(0),
                conns: Mutex::new(Vec::new()),
            }),
            sport: 1,
            dport: 2,
            state: Mutex::new(ConnState {
                tx_seq: 0,
                tx_ack: next,
                connected: true,
                closed: false,
                queue: VecDeque::new(),
                queued: 0,
                gap: false,
            }),
            ready: Condvar::new(),
        }
    }

    fn stream(c: &Conn) -> Vec<u8> {
        c.lock().queue.iter().flatten().copied().collect()
    }

    #[test]
    fn the_stream_takes_segments_in_order_only() {
        let c = conn(100);
        c.on_packet(TH_ACK, 100, b"abc");
        // One lost on the way: the next is refused until it comes again.
        c.on_packet(TH_ACK, 106, b"ghi");
        assert_eq!(stream(&c), b"abc");
        assert!(c.lock().gap);
        c.on_packet(TH_ACK, 103, b"def");
        // Sent again, now in its place; and once more, a duplicate.
        c.on_packet(TH_ACK, 106, b"ghi");
        c.on_packet(TH_ACK, 106, b"ghi");
        assert_eq!(stream(&c), b"abcdefghi");
        assert!(!c.lock().gap);
        // Overlapping what was had: only the new part.
        c.on_packet(TH_ACK, 107, b"hijk");
        assert_eq!(stream(&c), b"abcdefghijk");
        assert_eq!(c.lock().tx_ack, 111);
    }

    fn mux_header(len: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&P_TCP.to_be_bytes());
        h.extend_from_slice(&len.to_be_bytes());
        h.extend_from_slice(&MUX_MAGIC.to_be_bytes());
        h.extend_from_slice(&[0, 1, 0, 2]);
        h
    }

    #[test]
    fn an_ncm_block_in_the_mux_stream_is_skipped_whole() {
        // The block the board's controller handed to the mux: NTH16 with a
        // block length of 40, then a mux packet.
        let mut rx = b"NCMH\x0c\x00\x8e\x07".to_vec();
        rx.extend_from_slice(&40u16.to_le_bytes());
        rx.extend_from_slice(&[0x0c, 0x00]);
        rx.resize(40, 0xee);
        rx.extend_from_slice(&mux_header(16));
        assert_eq!(resync(&rx, 0), Resync::At(40));
        // Only part of it here yet: wait for the rest.
        assert_eq!(resync(&rx[..30], 0), Resync::Wait(0));
    }

    #[test]
    fn a_resync_finds_the_next_mux_header() {
        let mut rx = vec![0x55; 21];
        rx.extend_from_slice(&mux_header(16));
        assert_eq!(resync(&rx, 0), Resync::At(21));
        // A magic with a length no header has is not one.
        let mut rx = vec![0x55; 5];
        rx.extend_from_slice(&mux_header(3));
        rx.extend_from_slice(&[0x55; 4]);
        assert_eq!(resync(&rx, 0), Resync::Wait(rx.len() - 11));
    }

    #[test]
    fn sequence_numbers_wrap() {
        let c = conn(u32::MAX - 1);
        c.on_packet(TH_ACK, u32::MAX - 1, b"ab");
        c.on_packet(TH_ACK, 0, b"cd");
        assert_eq!(stream(&c), b"abcd");
        assert_eq!(c.lock().tx_ack, 2);
    }
}
