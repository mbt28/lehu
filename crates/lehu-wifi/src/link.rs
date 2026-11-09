// SPDX-License-Identifier: GPL-3.0-or-later
//! What the phone's Wi-Fi link does: the AP's operating channel and the
//! stations on it, asked of the kernel over nl80211 (generic netlink, no
//! privilege needed to read), and the interface's byte counters from sysfs.
//! Nothing is spawned: one request each, answered from the driver's own
//! numbers, cheap enough for once a second while a page shows them.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

// Netlink and generic netlink (linux/netlink.h, linux/genetlink.h).
const NETLINK_GENERIC: libc::c_int = 16;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const GENL_ID_CTRL: u16 = 16;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NLA_TYPE_MASK: u16 = 0x3fff;

// nl80211 (linux/nl80211.h).
const CMD_GET_INTERFACE: u8 = 5;
const CMD_GET_STATION: u8 = 17;
const ATTR_IFINDEX: u16 = 3;
const ATTR_MAC: u16 = 6;
const ATTR_STA_INFO: u16 = 21;
const ATTR_WIPHY_FREQ: u16 = 38;
const ATTR_CHANNEL_WIDTH: u16 = 159;
const STA_INACTIVE_TIME: u16 = 1;
const STA_RX_BYTES: u16 = 2;
const STA_TX_BYTES: u16 = 3;
const STA_SIGNAL: u16 = 7;
const STA_TX_BITRATE: u16 = 8;
const STA_RX_BITRATE: u16 = 14;
const STA_RX_BYTES64: u16 = 23;
const STA_TX_BYTES64: u16 = 24;
const RATE_BITRATE: u16 = 1;
const RATE_BITRATE32: u16 = 5;

/// A station on the AP: a phone.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Station {
    pub mac: [u8; 6],
    /// dBm; 0 when the driver gave none.
    pub signal_dbm: i32,
    /// The last frame's rate, in kbit/s; 0 when not given. Tx is the AP
    /// sending, to the phone.
    pub tx_kbps: u32,
    pub rx_kbps: u32,
    pub tx_bytes: u64,
    pub rx_bytes: u64,
    pub inactive_ms: u32,
}

/// The AP's channel as the radio runs it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Channel {
    pub mhz: u32,
    /// 0 when the driver does not say.
    pub width_mhz: u32,
}

/// The link as one sample sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Link {
    /// The interface has carrier: hostapd is beaconing.
    pub up: bool,
    /// The operating channel, when the driver reports one.
    pub channel: Option<Channel>,
    /// The busiest station, when one is associated.
    pub station: Option<Station>,
    pub stations: usize,
    /// The interface's counters since it came up.
    pub tx_bytes: u64,
    pub rx_bytes: u64,
}

/// A generic netlink socket with nl80211's family resolved.
pub struct Nl80211 {
    fd: OwnedFd,
    family: u16,
    seq: u32,
    buf: Vec<u8>,
}

impl Nl80211 {
    pub fn open() -> io::Result<Self> {
        // SAFETY: plain socket call; the fd is owned from here on.
        let raw = unsafe {
            libc::socket(
                libc::AF_NETLINK,
                libc::SOCK_RAW | libc::SOCK_CLOEXEC,
                NETLINK_GENERIC,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // A driver that never answers must not hold the loop thread.
        let timeout = libc::timeval {
            tv_sec: 0,
            tv_usec: 200_000,
        };
        // SAFETY: the option value is a live timeval of the size given.
        unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_RCVTIMEO,
                (&raw const timeout).cast(),
                std::mem::size_of::<libc::timeval>() as libc::socklen_t,
            );
        }
        let mut nl = Self {
            fd,
            family: GENL_ID_CTRL,
            seq: 0,
            buf: vec![0; 16384],
        };
        let mut attrs = Vec::new();
        put_attr(&mut attrs, CTRL_ATTR_FAMILY_NAME, b"nl80211\0");
        let mut family = None;
        nl.request(GENL_ID_CTRL, CTRL_CMD_GETFAMILY, 0, &attrs, |a| {
            if let Some(id) = find(a, CTRL_ATTR_FAMILY_ID).and_then(u16_of) {
                family = Some(id);
            }
        })?;
        nl.family = family.ok_or_else(|| io::Error::other("no nl80211 in this kernel"))?;
        Ok(nl)
    }

    /// The interface's operating channel; `None` when the driver gives none
    /// (an AP that is down, or a driver without the call).
    pub fn channel(&mut self, ifindex: u32) -> io::Result<Option<Channel>> {
        let mut attrs = Vec::new();
        put_attr(&mut attrs, ATTR_IFINDEX, &ifindex.to_ne_bytes());
        let mut out = None;
        self.request(self.family, CMD_GET_INTERFACE, 0, &attrs, |a| {
            out = channel_of(a);
        })?;
        Ok(out)
    }

    /// Every station on the interface.
    pub fn stations(&mut self, ifindex: u32, out: &mut Vec<Station>) -> io::Result<()> {
        out.clear();
        let mut attrs = Vec::new();
        put_attr(&mut attrs, ATTR_IFINDEX, &ifindex.to_ne_bytes());
        self.request(self.family, CMD_GET_STATION, NLM_F_DUMP, &attrs, |a| {
            if let Some(s) = station_of(a) {
                out.push(s);
            }
        })
    }

    /// Send one request and hand each answer's attributes to `each`, until
    /// the dump is done or the request acknowledged.
    fn request(
        &mut self,
        family: u16,
        cmd: u8,
        flags: u16,
        attrs: &[u8],
        mut each: impl FnMut(&[u8]),
    ) -> io::Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let seq = self.seq;
        let len = 16 + 4 + attrs.len();
        let mut msg = Vec::with_capacity(len);
        msg.extend_from_slice(&(len as u32).to_ne_bytes());
        msg.extend_from_slice(&family.to_ne_bytes());
        msg.extend_from_slice(&(NLM_F_REQUEST | NLM_F_ACK | flags).to_ne_bytes());
        msg.extend_from_slice(&seq.to_ne_bytes());
        msg.extend_from_slice(&0u32.to_ne_bytes());
        msg.extend_from_slice(&[cmd, 1, 0, 0]);
        msg.extend_from_slice(attrs);
        // SAFETY: a send of the bytes just built.
        let sent = unsafe { libc::send(self.fd.as_raw_fd(), msg.as_ptr().cast(), msg.len(), 0) };
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        loop {
            // SAFETY: a receive into our own buffer, within its length.
            let got = unsafe {
                libc::recv(
                    self.fd.as_raw_fd(),
                    self.buf.as_mut_ptr().cast(),
                    self.buf.len(),
                    0,
                )
            };
            if got < 0 {
                return Err(io::Error::last_os_error());
            }
            match walk(&self.buf[..got as usize], seq, &mut each) {
                Walk::More => {}
                Walk::Done => return Ok(()),
                Walk::Error(code) => return Err(io::Error::from_raw_os_error(code)),
            }
        }
    }
}

enum Walk {
    More,
    Done,
    Error(i32),
}

/// One datagram of netlink messages: answers to `seq` go to `each`; an
/// error message ends the walk (code 0 is the acknowledgement), as does
/// the end of a dump.
fn walk(mut data: &[u8], seq: u32, each: &mut impl FnMut(&[u8])) -> Walk {
    while data.len() >= 16 {
        let len = u32_at(data, 0) as usize;
        if len < 16 || len > data.len() {
            return Walk::Error(libc::EPROTO);
        }
        let kind = u16::from_ne_bytes([data[4], data[5]]);
        let ours = u32_at(data, 8) == seq;
        let body = &data[16..len];
        if ours {
            match kind {
                NLMSG_DONE => return Walk::Done,
                NLMSG_ERROR => {
                    let code = if body.len() >= 4 {
                        i32::from_ne_bytes([body[0], body[1], body[2], body[3]])
                    } else {
                        -libc::EPROTO
                    };
                    return if code == 0 {
                        Walk::Done
                    } else {
                        Walk::Error(-code)
                    };
                }
                _ if body.len() >= 4 => each(&body[4..]),
                _ => {}
            }
        }
        data = &data[align(len).min(data.len())..];
    }
    Walk::More
}

fn align(n: usize) -> usize {
    (n + 3) & !3
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn put_attr(out: &mut Vec<u8>, kind: u16, payload: &[u8]) {
    let len = 4 + payload.len();
    out.extend_from_slice(&(len as u16).to_ne_bytes());
    out.extend_from_slice(&kind.to_ne_bytes());
    out.extend_from_slice(payload);
    out.resize(align(out.len()), 0);
}

/// The attributes in `data`, each (type, payload); a malformed one ends it.
fn attrs(mut data: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        if data.len() < 4 {
            return None;
        }
        let len = u16::from_ne_bytes([data[0], data[1]]) as usize;
        if len < 4 || len > data.len() {
            return None;
        }
        let kind = u16::from_ne_bytes([data[2], data[3]]) & NLA_TYPE_MASK;
        let payload = &data[4..len];
        data = &data[align(len).min(data.len())..];
        Some((kind, payload))
    })
}

fn find(data: &[u8], kind: u16) -> Option<&[u8]> {
    attrs(data).find(|(k, _)| *k == kind).map(|(_, p)| p)
}

fn u16_of(p: &[u8]) -> Option<u16> {
    Some(u16::from_ne_bytes(p.get(..2)?.try_into().ok()?))
}

fn u32_of(p: &[u8]) -> Option<u32> {
    Some(u32::from_ne_bytes(p.get(..4)?.try_into().ok()?))
}

fn u64_of(p: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(p.get(..8)?.try_into().ok()?))
}

/// A rate_info nest in kbit/s (the kernel gives 100 kbit/s units).
fn rate_of(nest: &[u8]) -> u32 {
    let units = find(nest, RATE_BITRATE32)
        .and_then(u32_of)
        .or_else(|| find(nest, RATE_BITRATE).and_then(u16_of).map(u32::from))
        .unwrap_or(0);
    units.saturating_mul(100)
}

/// One GET_STATION answer.
fn station_of(msg: &[u8]) -> Option<Station> {
    let mac: [u8; 6] = find(msg, ATTR_MAC)?.get(..6)?.try_into().ok()?;
    let info = find(msg, ATTR_STA_INFO)?;
    let mut s = Station {
        mac,
        ..Station::default()
    };
    for (kind, p) in attrs(info) {
        match kind {
            STA_INACTIVE_TIME => s.inactive_ms = u32_of(p).unwrap_or(0),
            STA_SIGNAL => s.signal_dbm = p.first().map_or(0, |&b| i32::from(b as i8)),
            STA_TX_BITRATE => s.tx_kbps = rate_of(p),
            STA_RX_BITRATE => s.rx_kbps = rate_of(p),
            // The 64-bit counters win over the 32-bit ones that wrap.
            STA_RX_BYTES if s.rx_bytes == 0 => s.rx_bytes = u32_of(p).map_or(0, u64::from),
            STA_TX_BYTES if s.tx_bytes == 0 => s.tx_bytes = u32_of(p).map_or(0, u64::from),
            STA_RX_BYTES64 => s.rx_bytes = u64_of(p).unwrap_or(s.rx_bytes),
            STA_TX_BYTES64 => s.tx_bytes = u64_of(p).unwrap_or(s.tx_bytes),
            _ => {}
        }
    }
    Some(s)
}

/// One GET_INTERFACE answer's channel.
fn channel_of(msg: &[u8]) -> Option<Channel> {
    let mhz = find(msg, ATTR_WIPHY_FREQ).and_then(u32_of)?;
    let width_mhz = match find(msg, ATTR_CHANNEL_WIDTH).and_then(u32_of) {
        Some(0 | 1) => 20,
        Some(2) => 40,
        Some(3) => 80,
        Some(4 | 5) => 160,
        _ => 0,
    };
    Some(Channel { mhz, width_mhz })
}

/// A channel's number from its centre frequency.
pub fn channel_number(mhz: u32) -> u32 {
    match mhz {
        2484 => 14,
        2412..=2472 => (mhz - 2407) / 5,
        5000..=5900 => (mhz - 5000) / 5,
        _ => 0,
    }
}

fn counter(iface: &str, name: &str) -> u64 {
    fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{name}"))
        .ok()
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0)
}

/// Samples the link: the socket is opened on the first sample and kept.
pub struct LinkProbe {
    iface: String,
    nl: Option<Nl80211>,
    failed: bool,
    stations: Vec<Station>,
}

impl LinkProbe {
    pub fn new(iface: &str) -> Self {
        Self {
            iface: iface.to_string(),
            nl: None,
            failed: false,
            stations: Vec::new(),
        }
    }

    pub fn sample(&mut self) -> Link {
        let mut link = Link {
            up: crate::carrier(&self.iface).unwrap_or(false),
            tx_bytes: counter(&self.iface, "tx_bytes"),
            rx_bytes: counter(&self.iface, "rx_bytes"),
            ..Link::default()
        };
        let Ok(name) = std::ffi::CString::new(self.iface.as_str()) else {
            return link;
        };
        // SAFETY: a NUL-terminated name.
        let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
        if ifindex == 0 {
            return link;
        }
        if self.nl.is_none() && !self.failed {
            match Nl80211::open() {
                Ok(nl) => self.nl = Some(nl),
                Err(e) => {
                    // Said once: the page shows what sysfs has.
                    log::warn!("wifi: no nl80211 ({e})");
                    self.failed = true;
                }
            }
        }
        let Some(nl) = &mut self.nl else {
            return link;
        };
        link.channel = nl.channel(ifindex).unwrap_or(None);
        if nl.stations(ifindex, &mut self.stations).is_ok() {
            link.stations = self.stations.len();
            link.station = self
                .stations
                .iter()
                .max_by_key(|s| s.tx_bytes + s.rx_bytes)
                .cloned();
        }
        link
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nest(kind: u16, inner: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_attr(&mut out, kind, inner);
        out
    }

    #[test]
    fn a_station_answer_parses() {
        let mut rate = Vec::new();
        put_attr(&mut rate, RATE_BITRATE, &650u16.to_ne_bytes());
        put_attr(&mut rate, RATE_BITRATE32, &4333u32.to_ne_bytes());
        let mut info = Vec::new();
        put_attr(&mut info, STA_INACTIVE_TIME, &120u32.to_ne_bytes());
        put_attr(&mut info, STA_RX_BYTES, &7u32.to_ne_bytes());
        put_attr(&mut info, STA_SIGNAL, &[(-58i8) as u8]);
        info.extend(nest(STA_TX_BITRATE, &rate));
        let mut rx = Vec::new();
        put_attr(&mut rx, RATE_BITRATE, &240u16.to_ne_bytes());
        info.extend(nest(STA_RX_BITRATE, &rx));
        put_attr(&mut info, STA_TX_BYTES64, &5_000_000_000u64.to_ne_bytes());
        put_attr(&mut info, STA_RX_BYTES64, &9u64.to_ne_bytes());
        let mut msg = Vec::new();
        put_attr(&mut msg, ATTR_IFINDEX, &3u32.to_ne_bytes());
        put_attr(&mut msg, ATTR_MAC, &[1, 2, 3, 4, 5, 6]);
        msg.extend(nest(ATTR_STA_INFO, &info));
        let s = station_of(&msg).unwrap();
        assert_eq!(s.mac, [1, 2, 3, 4, 5, 6]);
        assert_eq!(s.signal_dbm, -58);
        assert_eq!(s.tx_kbps, 433_300);
        assert_eq!(s.rx_kbps, 24_000);
        assert_eq!(s.tx_bytes, 5_000_000_000);
        assert_eq!(s.rx_bytes, 9);
        assert_eq!(s.inactive_ms, 120);
        // Without a MAC or the info it is no station; cut short, no panic.
        assert_eq!(station_of(&nest(ATTR_STA_INFO, &info)), None);
        for cut in 0..msg.len() {
            let _ = station_of(&msg[..cut]);
        }
    }

    #[test]
    fn an_interface_answer_gives_the_channel() {
        let mut msg = Vec::new();
        put_attr(&mut msg, ATTR_WIPHY_FREQ, &5180u32.to_ne_bytes());
        put_attr(&mut msg, ATTR_CHANNEL_WIDTH, &3u32.to_ne_bytes());
        assert_eq!(
            channel_of(&msg),
            Some(Channel {
                mhz: 5180,
                width_mhz: 80
            })
        );
        assert_eq!(channel_of(&[]), None);
        assert_eq!(channel_number(5180), 36);
        assert_eq!(channel_number(5825), 165);
        assert_eq!(channel_number(2437), 6);
        assert_eq!(channel_number(2484), 14);
        assert_eq!(channel_number(60_000), 0);
    }

    fn header(len: usize, kind: u16, seq: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(&(len as u32).to_ne_bytes());
        h.extend_from_slice(&kind.to_ne_bytes());
        h.extend_from_slice(&0u16.to_ne_bytes());
        h.extend_from_slice(&seq.to_ne_bytes());
        h.extend_from_slice(&0u32.to_ne_bytes());
        h
    }

    #[test]
    fn a_datagram_is_walked_to_its_end() {
        let mut data = header(16 + 4 + 8, 30, 7);
        data.extend_from_slice(&[17, 1, 0, 0]);
        put_attr(&mut data, ATTR_IFINDEX, &3u32.to_ne_bytes());
        data.extend(header(16 + 4, 30, 6)); // someone else's
        data.extend_from_slice(&[17, 1, 0, 0]);
        let mut seen = 0;
        assert!(matches!(walk(&data, 7, &mut |_| seen += 1), Walk::More));
        assert_eq!(seen, 1);
        let mut done = header(20, NLMSG_DONE, 7);
        done.extend_from_slice(&0i32.to_ne_bytes());
        assert!(matches!(walk(&done, 7, &mut |_| {}), Walk::Done));
        let mut err = header(20, NLMSG_ERROR, 7);
        err.extend_from_slice(&(-libc::ENODEV).to_ne_bytes());
        assert!(matches!(
            walk(&err, 7, &mut |_| {}),
            Walk::Error(libc::ENODEV)
        ));
        let mut ack = header(20, NLMSG_ERROR, 7);
        ack.extend_from_slice(&0i32.to_ne_bytes());
        assert!(matches!(walk(&ack, 7, &mut |_| {}), Walk::Done));
        // A length past the datagram is refused, not read.
        assert!(matches!(
            walk(&header(64, 30, 7), 7, &mut |_| {}),
            Walk::Error(_)
        ));
    }

    /// Against the machine's own Wi-Fi, when it has one: run with
    /// `cargo test -p lehu-wifi -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn the_local_link() {
        let iface = std::env::var("LEHU_WIFI").unwrap_or_else(|_| "wlan0".into());
        let mut probe = LinkProbe::new(&iface);
        let link = probe.sample();
        println!("{link:#?}");
        assert!(probe.nl.is_some());
    }
}
