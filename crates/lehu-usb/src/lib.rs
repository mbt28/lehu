//! Which phones are on the USB bus, and when they come and go.
//!
//! The protocol crates open and drive a device themselves; this crate only
//! tells the main loop that one appeared or left, from the kernel's uevent
//! netlink socket (readable without privileges), with a scan of sysfs at
//! start for what is already plugged in. The one device-level operation
//! both protocols need, the port reset, is here as well.
//!
//! No udev: the board runs none for USB, and the kernel's own events carry
//! everything needed (bus, address, ids, class).

use std::fmt;
use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lehu_core::poll::{Interest, Poller};

/// A vendor and product id.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Id {
    pub vendor: u16,
    pub product: u16,
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// What a device is to the head unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A hub, the root hub included: never a phone.
    Hub,
    /// Apple's vendor id: an iPhone or iPad, for wired CarPlay.
    Apple,
    /// Google's accessory ids: a phone already in Android Open Accessory
    /// mode, for Android Auto.
    Accessory,
    /// Anything else; an Android phone before the accessory switch looks
    /// like this.
    Other,
}

const VENDOR_APPLE: u16 = 0x05ac;
const VENDOR_GOOGLE: u16 = 0x18d1;
const PRODUCT_ACCESSORY: u16 = 0x2d00;
const PRODUCT_ACCESSORY_ADB: u16 = 0x2d01;
const CLASS_HUB: u8 = 9;

/// The longest sysfs name kept: "1-1.4.2" is typical, deeper trees are
/// cut, and the name is only for the log.
const NAME_LEN: usize = 15;

/// A device on the bus, as the kernel describes it. Fixed size: it travels
/// through queues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Device {
    pub bus: u8,
    pub address: u8,
    pub id: Id,
    /// The device class from the descriptor; 9 is a hub.
    pub class: u8,
    name: [u8; NAME_LEN],
    name_len: u8,
}

impl Device {
    /// A device as the kernel would describe it; the scan and the uevent
    /// parser build them, tests too.
    pub fn new(bus: u8, address: u8, id: Id, class: u8, name: &str) -> Self {
        let mut d = Self {
            bus,
            address,
            id,
            class,
            name: [0; NAME_LEN],
            name_len: 0,
        };
        let n = name.len().min(NAME_LEN);
        d.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        d.name_len = n as u8;
        d
    }

    /// The sysfs name, such as "1-1".
    pub fn name(&self) -> &str {
        std::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("")
    }

    pub fn role(&self) -> Role {
        if self.class == CLASS_HUB {
            Role::Hub
        } else if self.id.vendor == VENDOR_APPLE {
            Role::Apple
        } else if self.id.vendor == VENDOR_GOOGLE
            && (self.id.product == PRODUCT_ACCESSORY || self.id.product == PRODUCT_ACCESSORY_ADB)
        {
            Role::Accessory
        } else {
            Role::Other
        }
    }

    /// The usbfs node the protocols open.
    pub fn node(&self) -> String {
        format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.address)
    }

    /// Whether `other` is the same port and address: the kernel reuses an
    /// address only after the device left.
    pub fn same_slot(&self, other: &Device) -> bool {
        self.bus == other.bus && self.address == other.address
    }
}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} at {}:{}",
            self.name(),
            self.id,
            self.bus,
            self.address
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Attached(Device),
    Detached(Device),
}

/// The devices on the bus now, hubs included, from sysfs.
pub fn scan() -> io::Result<Vec<Device>> {
    scan_in(Path::new("/sys/bus/usb/devices"))
}

fn scan_in(dir: &Path) -> io::Result<Vec<Device>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(d) = device_from_sysfs(&path) {
            found.push(d);
        }
    }
    found.sort_by_key(|d| (d.bus, d.address));
    Ok(found)
}

/// One sysfs device directory, or `None` for interfaces and anything else
/// without the descriptor files.
fn device_from_sysfs(dir: &Path) -> Option<Device> {
    let attr = |name: &str| fs::read_to_string(dir.join(name)).ok();
    let hex = |s: String| u16::from_str_radix(s.trim(), 16).ok();
    let dec = |s: String| s.trim().parse::<u8>().ok();
    let vendor = hex(attr("idVendor")?)?;
    let product = hex(attr("idProduct")?)?;
    let class = hex(attr("bDeviceClass")?)? as u8;
    let bus = dec(attr("busnum")?)?;
    let address = dec(attr("devnum")?)?;
    let name = dir.file_name()?.to_str()?;
    Some(Device::new(
        bus,
        address,
        Id { vendor, product },
        class,
        name,
    ))
}

/// Parse one kernel uevent datagram: "add@/devices/..." then NUL-separated
/// KEY=VALUE pairs. Only usb_device events of the usb subsystem count;
/// interfaces and other subsystems give `None`.
fn parse_uevent(buf: &[u8]) -> Option<Event> {
    let mut fields = buf.split(|&b| b == 0);
    let header = fields.next()?;
    let at = header.iter().position(|&b| b == b'@')?;
    let action = &header[..at];
    let path = std::str::from_utf8(&header[at + 1..]).ok()?;
    let name = path.rsplit('/').next()?;
    let (mut subsystem, mut devtype, mut product, mut kind, mut bus, mut address) =
        (None, None, None, None, None, None);
    for field in fields {
        let field = std::str::from_utf8(field).ok()?;
        if let Some((k, v)) = field.split_once('=') {
            match k {
                "SUBSYSTEM" => subsystem = Some(v),
                "DEVTYPE" => devtype = Some(v),
                "PRODUCT" => product = Some(v),
                "TYPE" => kind = Some(v),
                "BUSNUM" => bus = v.parse::<u8>().ok(),
                "DEVNUM" => address = v.parse::<u8>().ok(),
                _ => {}
            }
        }
    }
    if subsystem? != "usb" || devtype? != "usb_device" {
        return None;
    }
    // PRODUCT is "vid/pid/bcd" in unpadded hex; TYPE "class/sub/proto" in
    // decimal.
    let mut ids = product?.split('/');
    let vendor = u16::from_str_radix(ids.next()?, 16).ok()?;
    let pid = u16::from_str_radix(ids.next()?, 16).ok()?;
    let class = kind?.split('/').next()?.parse::<u8>().ok()?;
    let device = Device::new(
        bus?,
        address?,
        Id {
            vendor,
            product: pid,
        },
        class,
        name,
    );
    match action {
        b"add" => Some(Event::Attached(device)),
        b"remove" => Some(Event::Detached(device)),
        _ => None,
    }
}

/// Reset the device's port: the kernel re-enumerates it and the phone
/// starts its USB stack over. Used when a phone's own session is stuck
/// (CarPlay without a control connection) or an accessory was left behind
/// by a previous run. Needs write access to the node.
pub fn reset(device: &Device) -> io::Result<()> {
    // USBDEVFS_RESET is _IO('U', 20).
    const USBDEVFS_RESET: libc::c_ulong = 0x5514;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device.node())?;
    // SAFETY: the ioctl takes no argument; the fd is open for the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_RESET as _) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        // The device re-enumerated under the call and is gone from this
        // node: that is the reset having worked, not failed.
        if e.raw_os_error() == Some(libc::ENODEV) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

/// The hotplug thread: tells `on_event` about every usb_device add and
/// remove, after reporting what the scan found at start.
pub struct Watcher {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    pub fn start<F>(mut on_event: F) -> io::Result<Self>
    where
        F: FnMut(Event) + Send + 'static,
    {
        let socket = uevent_socket()?;
        // SAFETY: eventfd takes a count and flags; a negative result is an
        // error.
        let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor we own.
        let stop = unsafe { OwnedFd::from_raw_fd(fd) };
        let stop_fd = stop.as_raw_fd();
        let thread = thread::Builder::new()
            .name("usb-watch".into())
            .spawn(move || {
                // The socket is bound before the scan, so a device arriving
                // in between is seen twice at worst, never missed.
                match scan() {
                    Ok(devices) => {
                        for d in devices {
                            on_event(Event::Attached(d));
                        }
                    }
                    Err(e) => log::warn!("usb: scanning the bus: {e}"),
                }
                watch(socket, stop_fd, &mut on_event);
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    pub fn stop(mut self) {
        self.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn signal(&self) {
        let one: u64 = 1;
        // SAFETY: an eventfd write of exactly eight bytes from a local.
        unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                (&one as *const u64).cast(),
                std::mem::size_of::<u64>(),
            );
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.signal();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn uevent_socket() -> io::Result<OwnedFd> {
    // SAFETY: plain socket creation; the result is checked.
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // Group 1 is the kernel's broadcast; udev's own (group 2) is not wanted.
    // SAFETY: sockaddr_nl is plain data, zeroed then filled.
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = 1;
    // SAFETY: the address is a valid sockaddr_nl of the stated length.
    let rc = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&addr as *const libc::sockaddr_nl).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // A burst of events (a hub with devices behind it) must not overflow
    // the default buffer while the loop is busy; this much needs no
    // privilege.
    let size: libc::c_int = 256 * 1024;
    // SAFETY: setsockopt with an int of the stated size.
    unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
    Ok(socket)
}

fn watch<F: FnMut(Event)>(socket: OwnedFd, stop: libc::c_int, on_event: &mut F) {
    let mut poller = Poller::with_capacity(2);
    let Some(sock_index) = poller.add(socket.as_raw_fd(), Interest::Read) else {
        return;
    };
    let Some(stop_index) = poller.add(stop, Interest::Read) else {
        return;
    };
    // Kernel uevents are well under a page; udev's are not received.
    let mut buf = [0u8; 4096];
    loop {
        match poller.wait(None::<Duration>) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                log::warn!("usb: poll: {e}");
                return;
            }
        }
        if poller.readable(stop_index) || poller.gone(stop_index) {
            return;
        }
        if !poller.readable(sock_index) {
            continue;
        }
        loop {
            // SAFETY: recv into a local buffer of the stated length.
            let n = unsafe {
                libc::recv(
                    socket.as_raw_fd(),
                    buf.as_mut_ptr().cast(),
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                match e.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    // ENOBUFS: the kernel dropped events while we were
                    // away; the scan at the next session start covers it.
                    _ => {
                        log::warn!("usb: uevent socket: {e}");
                        break;
                    }
                }
            }
            if let Some(event) = parse_uevent(&buf[..n as usize]) {
                on_event(event);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uevent(action: &str, extra: &str) -> Vec<u8> {
        let mut s = format!("{action}@/devices/platform/soc/1c13000.usb/usb1/1-1\0");
        for f in [
            "ACTION=add",
            "DEVPATH=/devices/platform/soc/1c13000.usb/usb1/1-1",
            "SUBSYSTEM=usb",
            "MAJOR=189",
            "MINOR=1",
            "DEVNAME=bus/usb/001/002",
            "DEVTYPE=usb_device",
            "PRODUCT=5ac/12a8/1203",
            "TYPE=0/0/0",
            "BUSNUM=001",
            "DEVNUM=002",
            "SEQNUM=1234",
        ] {
            s.push_str(f);
            s.push('\0');
        }
        s.push_str(extra);
        s.into_bytes()
    }

    #[test]
    fn iphone_add_and_remove() {
        let Some(Event::Attached(d)) = parse_uevent(&uevent("add", "")) else {
            panic!("not an attach");
        };
        assert_eq!(
            d.id,
            Id {
                vendor: 0x05ac,
                product: 0x12a8
            }
        );
        assert_eq!((d.bus, d.address, d.class), (1, 2, 0));
        assert_eq!(d.name(), "1-1");
        assert_eq!(d.role(), Role::Apple);
        assert_eq!(d.node(), "/dev/bus/usb/001/002");
        assert_eq!(d.to_string(), "1-1 05ac:12a8 at 1:2");
        let Some(Event::Detached(r)) = parse_uevent(&uevent("remove", "")) else {
            panic!("not a detach");
        };
        assert!(r.same_slot(&d));
    }

    #[test]
    fn interfaces_and_other_subsystems_are_ignored() {
        let mut iface = uevent("add", "");
        let s = String::from_utf8(iface.clone()).unwrap();
        iface = s
            .replace("DEVTYPE=usb_device", "DEVTYPE=usb_interface")
            .into_bytes();
        assert_eq!(parse_uevent(&iface), None);
        let other = s.replace("SUBSYSTEM=usb", "SUBSYSTEM=input").into_bytes();
        assert_eq!(parse_uevent(&other), None);
        assert_eq!(
            parse_uevent(b"bind@/devices/x\0SUBSYSTEM=usb\0DEVTYPE=usb_device\0"),
            None
        );
        assert_eq!(parse_uevent(b""), None);
        assert_eq!(parse_uevent(b"add@"), None);
    }

    #[test]
    fn roles() {
        let dev = |v, p, c| {
            Device::new(
                1,
                3,
                Id {
                    vendor: v,
                    product: p,
                },
                c,
                "1-1",
            )
        };
        assert_eq!(dev(0x1d6b, 0x0002, 9).role(), Role::Hub);
        assert_eq!(dev(0x05ac, 0x12ab, 0).role(), Role::Apple);
        assert_eq!(dev(0x18d1, 0x2d00, 0).role(), Role::Accessory);
        assert_eq!(dev(0x18d1, 0x2d01, 0).role(), Role::Accessory);
        assert_eq!(dev(0x18d1, 0x4ee1, 0).role(), Role::Other);
        assert_eq!(dev(0x04e8, 0x6860, 0).role(), Role::Other);
    }

    #[test]
    fn long_names_are_cut_not_lost() {
        let d = Device::new(1, 3, Id::default(), 0, "1-1.4.2.3.1.2.5.6");
        assert_eq!(d.name(), "1-1.4.2.3.1.2.5");
    }

    #[test]
    fn sysfs_scan() {
        let root = std::env::temp_dir().join(format!("lehu-usb-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mk = |name: &str, attrs: &[(&str, &str)]| {
            let d = root.join(name);
            fs::create_dir_all(&d).unwrap();
            for (k, v) in attrs {
                fs::write(d.join(k), format!("{v}\n")).unwrap();
            }
        };
        mk(
            "usb1",
            &[
                ("idVendor", "1d6b"),
                ("idProduct", "0002"),
                ("bDeviceClass", "09"),
                ("busnum", "1"),
                ("devnum", "1"),
            ],
        );
        mk(
            "1-1",
            &[
                ("idVendor", "18d1"),
                ("idProduct", "2d01"),
                ("bDeviceClass", "00"),
                ("busnum", "1"),
                ("devnum", "7"),
            ],
        );
        mk("1-1:1.0", &[("bInterfaceClass", "ff")]);
        let found = scan_in(&root).unwrap();
        let _ = fs::remove_dir_all(&root);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].role(), Role::Hub);
        assert_eq!(found[1].role(), Role::Accessory);
        assert_eq!(found[1].address, 7);
        assert_eq!(found[1].name(), "1-1");
    }
}
