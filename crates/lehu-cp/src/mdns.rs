//! A minimal mDNS responder announcing exactly one service, the head unit's
//! `_airplay._tcp` on the control port, so an iPhone on the access point
//! finds it. It announces on start and answers PTR, SRV, TXT and A queries
//! that name the service, the instance or the host; a few hundred lines
//! instead of Avahi (no daemon, no D-Bus). Not a browser, not a resolver.
//! Answers allocate; they are rare.

use std::io;
use std::net::{Ipv4Addr, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const PORT: u16 = 5353;

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;
const CLASS_IN: u16 = 1;
/// The cache-flush bit on unique records.
const FLUSH: u16 = 0x8000;

/// A DNS name is at most this long on the wire.
const NAME_MAX: usize = 255;
/// A query with more questions than this is nothing a phone sends.
const QUESTIONS_MAX: u16 = 64;

/// What is announced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Service {
    /// The instance name, what the phone shows ("LEHU").
    pub instance: String,
    /// "_airplay._tcp": CarPlay is found as an AirPlay 2 receiver.
    pub service_type: String,
    /// The host the SRV record points at ("lehu.local").
    pub hostname: String,
    pub port: u16,
    /// Our address on the access point, the A record.
    pub ipv4: Ipv4Addr,
    /// "key=value" records.
    pub txt: Vec<String>,
}

impl Service {
    fn type_name(&self) -> String {
        format!("{}.local", self.service_type)
    }

    fn instance_name(&self) -> String {
        format!("{}.{}.local", self.instance, self.service_type)
    }
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_be_bytes());
}

fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_be_bytes());
}

/// A dotted name as length-prefixed labels and a zero. No compression
/// (valid, and simplest). A label over 63 bytes is cut: nothing we
/// announce is that long.
fn put_name(b: &mut Vec<u8>, name: &str) {
    for label in name.split('.').filter(|l| !l.is_empty()) {
        let bytes = &label.as_bytes()[..label.len().min(63)];
        b.push(bytes.len() as u8);
        b.extend_from_slice(bytes);
    }
    b.push(0);
}

fn put_record(b: &mut Vec<u8>, name: &str, ty: u16, class: u16, ttl: u32, rdata: &[u8]) {
    put_name(b, name);
    put16(b, ty);
    put16(b, class);
    put32(b, ttl);
    put16(b, rdata.len().min(usize::from(u16::MAX)) as u16);
    b.extend_from_slice(&rdata[..rdata.len().min(usize::from(u16::MAX))]);
}

/// The one answer: PTR, SRV, TXT and A, as a response to `id` (0 when
/// unsolicited).
pub fn build_answer(id: u16, s: &Service) -> Vec<u8> {
    let type_name = s.type_name();
    let instance = s.instance_name();
    let mut pkt = Vec::with_capacity(512);
    put16(&mut pkt, id);
    // Response, authoritative.
    put16(&mut pkt, 0x8400);
    put16(&mut pkt, 0);
    put16(&mut pkt, 4);
    put16(&mut pkt, 0);
    put16(&mut pkt, 0);

    let mut rd = Vec::with_capacity(256);
    put_name(&mut rd, &instance);
    put_record(&mut pkt, &type_name, TYPE_PTR, CLASS_IN, 4500, &rd);

    rd.clear();
    put16(&mut rd, 0);
    put16(&mut rd, 0);
    put16(&mut rd, s.port);
    put_name(&mut rd, &s.hostname);
    put_record(&mut pkt, &instance, TYPE_SRV, CLASS_IN | FLUSH, 120, &rd);

    rd.clear();
    if s.txt.is_empty() {
        // An empty TXT is a single zero-length string.
        rd.push(0);
    }
    for t in &s.txt {
        let bytes = &t.as_bytes()[..t.len().min(255)];
        rd.push(bytes.len() as u8);
        rd.extend_from_slice(bytes);
    }
    put_record(&mut pkt, &instance, TYPE_TXT, CLASS_IN | FLUSH, 4500, &rd);

    rd.clear();
    rd.extend_from_slice(&s.ipv4.octets());
    put_record(&mut pkt, &s.hostname, TYPE_A, CLASS_IN | FLUSH, 120, &rd);
    pkt
}

/// The lower-cased dotted name at `off`, following compression pointers,
/// and how many bytes the name took in place (two for a pointer). `None`
/// when it runs past the packet or loops.
fn read_name(pkt: &[u8], off: usize) -> Option<(String, usize)> {
    let mut name = String::with_capacity(64);
    let mut p = off;
    let mut consumed = None;
    // A pointer may point anywhere; this many hops is a loop.
    for _ in 0..64 {
        let len = usize::from(*pkt.get(p)?);
        if len == 0 {
            return Some((name, consumed.unwrap_or_else(|| p + 1 - off)));
        }
        if len & 0xc0 == 0xc0 {
            let lo = usize::from(*pkt.get(p + 1)?);
            if consumed.is_none() {
                consumed = Some(p + 2 - off);
            }
            p = ((len & 0x3f) << 8) | lo;
            continue;
        }
        if len & 0xc0 != 0 {
            return None;
        }
        let label = pkt.get(p + 1..p + 1 + len)?;
        if name.len() + 1 + len > NAME_MAX {
            return None;
        }
        if !name.is_empty() {
            name.push('.');
        }
        name.extend(label.iter().map(|b| b.to_ascii_lowercase() as char));
        p += 1 + len;
    }
    None
}

/// The query's id when one of its questions names our service, instance or
/// host; `None` for anything else, including responses and malformed
/// packets.
pub fn asks_for(pkt: &[u8], s: &Service) -> Option<u16> {
    if pkt.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if flags & 0x8000 != 0 {
        return None;
    }
    let questions = u16::from_be_bytes([pkt[4], pkt[5]]).min(QUESTIONS_MAX);
    let wanted = [
        s.type_name().to_ascii_lowercase(),
        s.instance_name().to_ascii_lowercase(),
        s.hostname.to_ascii_lowercase(),
    ];
    let mut off = 12;
    for _ in 0..questions {
        let (name, used) = read_name(pkt, off)?;
        // The type and class follow the name.
        off = off.checked_add(used)?.checked_add(4)?;
        if off > pkt.len() {
            return None;
        }
        if wanted.contains(&name) {
            return Some(u16::from_be_bytes([pkt[0], pkt[1]]));
        }
    }
    None
}

/// The responder's thread; dropping it stops the announcements.
pub struct Responder {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Responder {
    /// Open the multicast socket on the interface that owns `ipv4` and
    /// start answering.
    pub fn start(service: Service) -> io::Result<Responder> {
        let socket = open_socket(service.ipv4)?;
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let stop = Arc::clone(&stop);
            thread::Builder::new()
                .name("cp-mdns".into())
                .spawn(move || run(socket, service, stop))?
        };
        Ok(Responder {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A UDP socket on :5353 that shares the port with any other responder
/// (SO_REUSEADDR and SO_REUSEPORT) and is in the mDNS group on the AP's
/// interface.
fn open_socket(ipv4: Ipv4Addr) -> io::Result<UdpSocket> {
    // SAFETY: plain socket call; the descriptor is checked and then owned.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor we own.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    let one: libc::c_int = 1;
    for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
        // SAFETY: a live socket and a live int of the stated size.
        unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                (&one as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }
    let addr = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: PORT.to_be(),
        sin_addr: libc::in_addr {
            s_addr: u32::from_ne_bytes(Ipv4Addr::UNSPECIFIED.octets()),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: a live socket and a live sockaddr_in of the stated size.
    let r = unsafe {
        libc::bind(
            socket.as_raw_fd(),
            (&addr as *const libc::sockaddr_in).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    let iface = if ipv4.is_unspecified() {
        Ipv4Addr::UNSPECIFIED
    } else {
        ipv4
    };
    socket.join_multicast_v4(&GROUP, &iface)?;
    if !iface.is_unspecified() {
        // Announcements leave on the AP's interface, whatever the default
        // route is.
        let addr = libc::in_addr {
            s_addr: u32::from_ne_bytes(iface.octets()),
        };
        // SAFETY: a live socket and a live in_addr of the stated size.
        let r = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::IPPROTO_IP,
                libc::IP_MULTICAST_IF,
                (&addr as *const libc::in_addr).cast(),
                std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    socket.set_multicast_ttl_v4(255)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    Ok(socket)
}

fn run(socket: UdpSocket, service: Service, stop: Arc<AtomicBool>) {
    let group = (GROUP, PORT);
    log::info!(
        "mdns: announcing {}.{} on :{} at {}",
        service.instance,
        service.service_type,
        service.port,
        service.ipv4
    );
    // Announce a few times on start, unsolicited.
    let announce = build_answer(0, &service);
    for _ in 0..3 {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let _ = socket.send_to(&announce, group);
        thread::sleep(Duration::from_millis(250));
    }
    let mut buf = [0u8; 2048];
    while !stop.load(Ordering::Acquire) {
        let n = match socket.recv_from(&mut buf) {
            Ok((n, _)) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => {
                log::warn!("mdns: receive failed ({e}); the responder stops");
                return;
            }
        };
        if let Some(id) = asks_for(&buf[..n], &service) {
            let _ = socket.send_to(&build_answer(id, &service), group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> Service {
        Service {
            instance: "LEHU".into(),
            service_type: "_airplay._tcp".into(),
            hostname: "lehu.local".into(),
            port: 7000,
            ipv4: Ipv4Addr::new(192, 168, 53, 1),
            txt: vec![
                "deviceid=02:11:22:33:44:55".into(),
                "features=0x44540380,0x61".into(),
            ],
        }
    }

    /// A query for `names`, one question each, with the given id.
    fn query(id: u16, names: &[&str]) -> Vec<u8> {
        let mut q = Vec::new();
        put16(&mut q, id);
        put16(&mut q, 0);
        put16(&mut q, names.len() as u16);
        put16(&mut q, 0);
        put16(&mut q, 0);
        put16(&mut q, 0);
        for n in names {
            put_name(&mut q, n);
            put16(&mut q, TYPE_PTR);
            put16(&mut q, CLASS_IN);
        }
        q
    }

    #[test]
    fn the_answer_has_the_four_records() {
        let s = service();
        let a = build_answer(0x1234, &s);
        assert_eq!(&a[..12], &[0x12, 0x34, 0x84, 0x00, 0, 0, 0, 4, 0, 0, 0, 0]);
        // The PTR record: the type name, then PTR IN, then the instance.
        let (name, used) = read_name(&a, 12).unwrap();
        assert_eq!(name, "_airplay._tcp.local");
        let p = 12 + used;
        assert_eq!(&a[p..p + 4], &[0, 12, 0, 1]);
        let rdlen = usize::from(u16::from_be_bytes([a[p + 8], a[p + 9]]));
        let (target, _) = read_name(&a[..p + 10 + rdlen], p + 10).unwrap();
        assert_eq!(target, "lehu._airplay._tcp.local");
        // The SRV carries the port and the host.
        let srv = p + 10 + rdlen;
        let (name, used) = read_name(&a, srv).unwrap();
        assert_eq!(name, "lehu._airplay._tcp.local");
        let q = srv + used;
        assert_eq!(&a[q..q + 4], &[0, 33, 0x80, 1]);
        assert_eq!(&a[q + 10..q + 16], &[0, 0, 0, 0, 0x1b, 0x58]);
        // The TXT records and the A record end the packet.
        let txt_pos = a
            .windows(26)
            .position(|w| w == b"deviceid=02:11:22:33:44:55")
            .unwrap();
        assert_eq!(a[txt_pos - 1], 26);
        assert_eq!(&a[a.len() - 4..], &[192, 168, 53, 1]);
        // An empty TXT set is one empty string.
        let mut e = service();
        e.txt.clear();
        let b = build_answer(0, &e);
        assert!(b.len() < a.len());
    }

    #[test]
    fn queries_for_our_names_are_answered_and_nothing_else() {
        let s = service();
        assert_eq!(asks_for(&query(7, &["_airplay._tcp.local"]), &s), Some(7));
        assert_eq!(
            asks_for(&query(8, &["LEHU._AirPlay._tcp.local"]), &s),
            Some(8)
        );
        assert_eq!(
            asks_for(&query(9, &["other.local", "Lehu.Local"]), &s),
            Some(9)
        );
        assert_eq!(asks_for(&query(1, &["_raop._tcp.local"]), &s), None);
        assert_eq!(asks_for(&query(1, &[]), &s), None);
        // A response is not a query.
        let mut r = query(1, &["_airplay._tcp.local"]);
        r[2] = 0x84;
        assert_eq!(asks_for(&r, &s), None);
    }

    #[test]
    fn malformed_queries_are_dropped() {
        let s = service();
        assert_eq!(asks_for(&[], &s), None);
        assert_eq!(asks_for(&[0; 11], &s), None);
        // A name running past the end.
        let mut q = query(1, &["_airplay._tcp.local"]);
        q.truncate(20);
        assert_eq!(asks_for(&q, &s), None);
        // A question count beyond the bytes.
        let mut q = query(1, &["_airplay._tcp.local"]);
        q[5] = 200;
        assert_eq!(
            asks_for(&q, &s),
            Some(1),
            "the first question still matches"
        );
        let mut q = query(1, &["x.local"]);
        q[5] = 200;
        assert_eq!(asks_for(&q, &s), None);
        // A label length that overruns.
        let mut q = query(1, &["_airplay._tcp.local"]);
        q[12] = 200;
        assert_eq!(asks_for(&q, &s), None);
        // A pointer loop.
        let mut q = query(1, &[]);
        q[5] = 1;
        q.extend_from_slice(&[0xc0, 12, 0, 12, 0, 1]);
        assert_eq!(asks_for(&q, &s), None);
        // A pointer past the end.
        let mut q = query(1, &[]);
        q[5] = 1;
        q.extend_from_slice(&[0xc3, 0xff, 0, 12, 0, 1]);
        assert_eq!(asks_for(&q, &s), None);
        // The reserved label bits.
        let mut q = query(1, &[]);
        q[5] = 1;
        q.extend_from_slice(&[0x80, 0, 0, 12, 0, 1]);
        assert_eq!(asks_for(&q, &s), None);
        // A name longer than DNS allows, by pointers chaining back.
        let mut q = query(1, &[]);
        q[5] = 1;
        let mut long = Vec::new();
        for _ in 0..5 {
            long.push(63);
            long.extend_from_slice(&[b'a'; 63]);
        }
        long.push(0);
        q.extend_from_slice(&long);
        q.extend_from_slice(&[0, 12, 0, 1]);
        assert_eq!(asks_for(&q, &s), None);
        // A compressed name that is ours still matches.
        let mut q = query(1, &["_airplay._tcp.local"]);
        q[5] = 2;
        q.extend_from_slice(&[4, b'l', b'e', b'h', b'u', 0xc0, 12, 0, 12, 0, 1]);
        assert_eq!(asks_for(&q, &s), Some(1));
    }
}
