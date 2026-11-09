// SPDX-License-Identifier: GPL-3.0-or-later
//! iAP2, the protocol an accessory identifies itself with: link packets
//! with their two checksums, the link synchronisation that opens a link,
//! and control-session messages (CSMs). The link runs over any reliable
//! byte stream: the carkit TLS stream over usbmux, or a Bluetooth RFCOMM
//! socket for a wireless phone.
//!
//! The transport delivers bytes in order, so the link keeps only what the
//! phone expects of it: every data packet is acknowledged, a duplicate
//! (the phone retransmitting) is acknowledged and not delivered twice, and
//! our own last data packet is sent again when its acknowledgement is
//! overdue, up to the negotiated count.
//!
//! The link packet, link synchronisation and CSM codecs follow LIVI's
//! iap2/link_layer.py and iap2-link (Lasse Heitgres, GPL-3.0), through
//! FastCarPlay's cp_iap2.cpp and cp_iap2_link.cpp; the retransmission and
//! duplicate handling are LEHU's.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

pub const CONTROL_SYN: u8 = 0x80;
pub const CONTROL_ACK: u8 = 0x40;
pub const CONTROL_EAK: u8 = 0x20;

const LINK_START: [u8; 2] = [0xff, 0x5a];
const CSM_START: [u8; 2] = [0x40, 0x40];
const LSP_VERSION: u8 = 0x01;

/// What the accessory sends first: "I speak iAP2".
pub const DETECT_MARKER: [u8; 6] = [0xff, 0x55, 0x02, 0x00, 0xee, 0x10];

/// The control session's id, where CSMs travel.
pub const CONTROL_SESSION: u8 = 10;

/// The session kind of a file transfer (the album art), as the link
/// synchronisation names it.
pub const FILE_TRANSFER_KIND: u8 = 1;

/// A packet is at most this long; the negotiated maximum is lower.
const MAX_PACKET: usize = 65535;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub length: u16,
    pub control: u8,
    pub seq: u8,
    pub ack: u8,
    pub session: u8,
}

/// The byte that brings an 8-bit sum to zero.
fn checksum(data: &[u8]) -> u8 {
    data.iter()
        .fold(0u8, |s, &b| s.wrapping_add(b))
        .wrapping_neg()
}

/// A whole link packet: the 9-byte header with its checksum, then the
/// payload with its own.
pub fn pack_packet(control: u8, seq: u8, ack: u8, session: u8, payload: &[u8]) -> Vec<u8> {
    let length = if payload.is_empty() {
        9
    } else {
        payload.len() + 10
    } as u16;
    let mut pkt = Vec::with_capacity(usize::from(length));
    pkt.extend_from_slice(&LINK_START);
    pkt.extend_from_slice(&length.to_be_bytes());
    pkt.extend_from_slice(&[control, seq, ack, session]);
    pkt.push(checksum(&pkt));
    if !payload.is_empty() {
        pkt.extend_from_slice(payload);
        pkt.push(checksum(payload));
    }
    pkt
}

/// Parse one whole packet; `None` on a bad marker, length or checksum.
pub fn parse_packet(pkt: &[u8]) -> Option<(Header, &[u8])> {
    if pkt.len() < 9 || pkt[..2] != LINK_START || checksum(&pkt[..8]) != pkt[8] {
        return None;
    }
    let h = Header {
        length: u16::from_be_bytes([pkt[2], pkt[3]]),
        control: pkt[4],
        seq: pkt[5],
        ack: pkt[6],
        session: pkt[7],
    };
    if usize::from(h.length) != pkt.len() {
        return None;
    }
    if h.length == 9 {
        return Some((h, &[]));
    }
    if h.length < 10 {
        return None;
    }
    let payload = &pkt[9..pkt.len() - 1];
    if checksum(payload) != pkt[pkt.len() - 1] {
        return None;
    }
    Some((h, payload))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionDesc {
    pub id: u8,
    pub kind: u8,
    pub version: u8,
}

/// The link synchronisation payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkSync {
    pub max_outgoing: u8,
    pub max_len: u16,
    pub retransmission_timeout_ms: u16,
    pub ack_timeout_ms: u16,
    pub max_retransmissions: u8,
    pub max_ack: u8,
    pub sessions: Vec<SessionDesc>,
}

impl LinkSync {
    /// What the accessory proposes; the phone's reply is authoritative.
    /// The sessions are what an iPhone expects of a CarPlay accessory:
    /// control, external accessory and file transfer.
    pub fn proposal() -> Self {
        Self {
            max_outgoing: 4,
            max_len: 1500,
            retransmission_timeout_ms: 4000,
            ack_timeout_ms: 1000,
            max_retransmissions: 4,
            max_ack: 3,
            sessions: vec![
                SessionDesc {
                    id: CONTROL_SESSION,
                    kind: 0,
                    version: 1,
                },
                SessionDesc {
                    id: 11,
                    kind: 2,
                    version: 1,
                },
                SessionDesc {
                    id: 12,
                    kind: 1,
                    version: 2,
                },
            ],
        }
    }

    pub fn pack(&self) -> Vec<u8> {
        let mut p = vec![LSP_VERSION, self.max_outgoing];
        p.extend_from_slice(&self.max_len.to_be_bytes());
        p.extend_from_slice(&self.retransmission_timeout_ms.to_be_bytes());
        p.extend_from_slice(&self.ack_timeout_ms.to_be_bytes());
        p.push(self.max_retransmissions);
        p.push(self.max_ack);
        for s in &self.sessions {
            p.extend_from_slice(&[s.id, s.kind, s.version]);
        }
        p
    }

    pub fn parse(p: &[u8]) -> Option<Self> {
        if p.len() < 10 || p[0] != LSP_VERSION {
            return None;
        }
        Some(Self {
            max_outgoing: p[1],
            max_len: u16::from_be_bytes([p[2], p[3]]),
            retransmission_timeout_ms: u16::from_be_bytes([p[4], p[5]]),
            ack_timeout_ms: u16::from_be_bytes([p[6], p[7]]),
            max_retransmissions: p[8],
            max_ack: p[9],
            sessions: p[10..]
                .as_chunks::<3>()
                .0
                .iter()
                .map(|c| SessionDesc {
                    id: c[0],
                    kind: c[1],
                    version: c[2],
                })
                .collect(),
        })
    }
}

/// A control-session message: `[0x4040][length][id]` then parameters, each
/// `[length][id][value]` with the length counting its own four bytes.
pub fn pack_csm(id: u16, params: &[(u16, &[u8])]) -> Vec<u8> {
    let body = pack_params(params);
    let mut msg = Vec::with_capacity(6 + body.len());
    msg.extend_from_slice(&CSM_START);
    msg.extend_from_slice(&((6 + body.len()) as u16).to_be_bytes());
    msg.extend_from_slice(&id.to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// Parameters alone: a group parameter's value is one of these.
pub fn pack_params(params: &[(u16, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for &(id, value) in params {
        body.extend_from_slice(&((4 + value.len()) as u16).to_be_bytes());
        body.extend_from_slice(&id.to_be_bytes());
        body.extend_from_slice(value);
    }
    body
}

/// Split a parameter body; `None` when a length runs past the end.
pub fn parse_params(body: &[u8]) -> Option<Params<'_>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= body.len() {
        let len = usize::from(u16::from_be_bytes([body[p], body[p + 1]]));
        let id = u16::from_be_bytes([body[p + 2], body[p + 3]]);
        if len < 4 || p + len > body.len() {
            return None;
        }
        out.push((id, &body[p + 4..p + len]));
        p += len;
    }
    (p == body.len()).then_some(out)
}

/// A message's parameters: id and value.
pub type Params<'a> = Vec<(u16, &'a [u8])>;

pub fn parse_csm(msg: &[u8]) -> Option<(u16, Params<'_>)> {
    if msg.len() < 6 || msg[..2] != CSM_START {
        return None;
    }
    let length = usize::from(u16::from_be_bytes([msg[2], msg[3]]));
    if length < 6 || length > msg.len() {
        return None;
    }
    let id = u16::from_be_bytes([msg[4], msg[5]]);
    Some((id, parse_params(&msg[6..length])?))
}

// ── The link ──────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Negotiate,
    Normal,
    Dead,
}

/// What `recv_control` found.
pub enum Received {
    /// A control-session message, in the buffer given.
    Control,
    /// A file transfer datagram, in the buffer given.
    File,
    /// Nothing within the stream's read timeout.
    Nothing,
}

/// The link over a stream whose `read` may time out (`WouldBlock` or
/// `TimedOut`): a timeout is a quiet moment, not an error, so the caller
/// gets to run its own timers between packets.
pub struct Link<S: Read + Write> {
    stream: S,
    state: State,
    /// Our sequence number; incremented before each data packet.
    sent_psn: u8,
    /// The last in-sequence sequence number received: our ack field.
    last_recv_psn: u8,
    sync: LinkSync,
    rx: Vec<u8>,
    /// Our last data packet, until the phone acknowledges it.
    pending: Option<Pending>,
}

struct Pending {
    psn: u8,
    packet: Vec<u8>,
    sent: Instant,
    resends: u8,
}

impl<S: Read + Write> Link<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            state: State::Negotiate,
            sent_psn: 99,
            last_recv_psn: 0,
            sync: LinkSync::proposal(),
            rx: Vec::with_capacity(4096),
            pending: None,
        }
    }

    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn alive(&self) -> bool {
        self.state != State::Dead
    }

    pub fn sync(&self) -> &LinkSync {
        &self.sync
    }

    /// The file transfer session, when the negotiated link has one.
    pub fn file_session(&self) -> Option<u8> {
        self.sync
            .sessions
            .iter()
            .find(|s| s.kind == FILE_TRANSFER_KIND)
            .map(|s| s.id)
    }

    fn write_packet(
        &mut self,
        control: u8,
        seq: u8,
        session: u8,
        payload: &[u8],
    ) -> io::Result<Vec<u8>> {
        let pkt = pack_packet(control, seq, self.last_recv_psn, session, payload);
        if let Err(e) = self.stream.write_all(&pkt) {
            self.state = State::Dead;
            return Err(e);
        }
        Ok(pkt)
    }

    fn send_ack(&mut self) -> io::Result<()> {
        self.write_packet(CONTROL_ACK, self.sent_psn, 0, &[])
            .map(drop)
    }

    /// One packet out of the receive buffer, reading more when none is
    /// whole. Stray bytes before a start marker (a detect marker, noise)
    /// are skipped. `Ok(None)` on a read timeout.
    fn read_packet(&mut self) -> io::Result<Option<(Header, Vec<u8>)>> {
        loop {
            // Resync to the start marker.
            if let Some(p) = self.rx.windows(2).position(|w| w == LINK_START) {
                if p > 0 {
                    self.rx.drain(..p);
                }
            } else {
                let keep = usize::from(self.rx.last() == Some(&LINK_START[0]));
                let drop = self.rx.len() - keep;
                self.rx.drain(..drop);
            }
            if self.rx.len() >= 9 {
                let length = usize::from(u16::from_be_bytes([self.rx[2], self.rx[3]]));
                if !(9..=MAX_PACKET).contains(&length) {
                    // Not a packet after all: step past the false marker.
                    self.rx.drain(..2);
                    continue;
                }
                if self.rx.len() >= length {
                    let parsed = parse_packet(&self.rx[..length]).map(|(h, p)| (h, p.to_vec()));
                    match parsed {
                        Some(found) => {
                            self.rx.drain(..length);
                            return Ok(Some(found));
                        }
                        None => {
                            self.rx.drain(..2);
                            continue;
                        }
                    }
                }
            }
            let mut buf = [0u8; 2048];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.state = State::Dead;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "iAP2 link closed",
                    ));
                }
                Ok(n) => {
                    if self.rx.len() + n > 2 * MAX_PACKET {
                        self.rx.clear();
                    }
                    self.rx.extend_from_slice(&buf[..n]);
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => {
                    self.state = State::Dead;
                    return Err(e);
                }
            }
        }
    }

    /// Open the link: the detect marker, our SYN, then the phone's SYN
    /// (its parameters win) and the ACK that makes the link normal. Gives
    /// up at `deadline`.
    pub fn negotiate(&mut self, deadline: Instant) -> io::Result<()> {
        self.state = State::Negotiate;
        // A real iPhone answers the marker with its own SYN packet, never
        // with the marker, so ours goes out without waiting for one.
        if let Err(e) = self.stream.write_all(&DETECT_MARKER) {
            self.state = State::Dead;
            return Err(e);
        }
        let proposal = self.sync.pack();
        self.write_packet(CONTROL_SYN, self.sent_psn, 0, &proposal)?;
        while self.state == State::Negotiate {
            if Instant::now() >= deadline {
                self.state = State::Dead;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "iAP2 negotiation timed out",
                ));
            }
            let Some((h, payload)) = self.read_packet()? else {
                continue;
            };
            if h.control & CONTROL_SYN != 0 {
                if let Some(peer) = LinkSync::parse(&payload) {
                    self.sync = peer;
                }
                self.last_recv_psn = h.seq;
                self.send_ack()?;
            }
            if h.control & CONTROL_ACK != 0 {
                self.state = State::Normal;
            }
        }
        log::info!(
            "iap2: link up (max {} bytes, {} sessions)",
            self.sync.max_len,
            self.sync.sessions.len()
        );
        Ok(())
    }

    /// Send a CSM on the control session.
    pub fn send_control(&mut self, csm: &[u8]) -> io::Result<()> {
        self.send_on(CONTROL_SESSION, csm)
    }

    /// Send a datagram on the file transfer session.
    pub fn send_file(&mut self, datagram: &[u8]) -> io::Result<()> {
        let Some(session) = self.file_session() else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no iAP2 file transfer session",
            ));
        };
        self.send_on(session, datagram)
    }

    /// A data packet on `session`. Only the newest waits for its ACK: the
    /// phone acknowledges cumulatively, over a transport that keeps order.
    fn send_on(&mut self, session: u8, payload: &[u8]) -> io::Result<()> {
        if self.state != State::Normal {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "iAP2 link not up",
            ));
        }
        self.sent_psn = self.sent_psn.wrapping_add(1);
        let pkt = self.write_packet(CONTROL_ACK, self.sent_psn, session, payload)?;
        self.pending = Some(Pending {
            psn: self.sent_psn,
            packet: pkt,
            sent: Instant::now(),
            resends: 0,
        });
        Ok(())
    }

    /// Send the pending data packet again when its ACK is overdue; the
    /// link dies after the negotiated number of tries.
    fn retransmit_if_due(&mut self) -> io::Result<()> {
        let timeout =
            Duration::from_millis(u64::from(self.sync.retransmission_timeout_ms.max(500)));
        let max = self.sync.max_retransmissions;
        let Some(p) = &mut self.pending else {
            return Ok(());
        };
        if p.sent.elapsed() < timeout {
            return Ok(());
        }
        if p.resends >= max {
            self.state = State::Dead;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "iAP2 packet never acknowledged",
            ));
        }
        p.resends += 1;
        p.sent = Instant::now();
        let packet = p.packet.clone();
        log::debug!("iap2: sending packet {} again", p.psn);
        if let Err(e) = self.stream.write_all(&packet) {
            self.state = State::Dead;
            return Err(e);
        }
        Ok(())
    }

    /// The next CSM or file transfer datagram from the phone into `out`,
    /// acknowledged. Pure ACKs, other sessions' data and duplicates are
    /// consumed on the way.
    pub fn recv_control(&mut self, out: &mut Vec<u8>) -> io::Result<Received> {
        while self.state == State::Normal {
            let Some((h, payload)) = self.read_packet()? else {
                self.retransmit_if_due()?;
                return Ok(Received::Nothing);
            };
            if let Some(p) = &self.pending
                && h.control & CONTROL_ACK != 0
                && h.ack == p.psn
            {
                self.pending = None;
            }
            let is_data = h.control & !CONTROL_ACK == 0 && !payload.is_empty();
            if !is_data {
                if h.control & CONTROL_EAK != 0 {
                    // The phone names what it is missing: resend ours now.
                    if let Some(p) = &mut self.pending {
                        p.sent = Instant::now() - Duration::from_secs(60);
                    }
                    self.retransmit_if_due()?;
                }
                continue;
            }
            if h.seq == self.last_recv_psn {
                // The phone did not see our ACK: say it again, deliver once.
                self.send_ack()?;
                continue;
            }
            self.last_recv_psn = h.seq;
            self.send_ack()?;
            if h.session == CONTROL_SESSION {
                out.clear();
                out.extend_from_slice(&payload);
                return Ok(Received::Control);
            }
            if Some(h.session) == self.file_session() {
                out.clear();
                out.extend_from_slice(&payload);
                return Ok(Received::File);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "iAP2 link dead",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    #[test]
    fn packets_round_trip_and_checksums_catch_corruption() {
        let pkt = pack_packet(CONTROL_ACK, 7, 3, CONTROL_SESSION, b"abc");
        assert_eq!(pkt.len(), 13);
        let (h, p) = parse_packet(&pkt).unwrap();
        assert_eq!(
            (h.control, h.seq, h.ack, h.session, h.length),
            (CONTROL_ACK, 7, 3, 10, 13)
        );
        assert_eq!(p, b"abc");
        let mut bad = pkt.clone();
        bad[10] ^= 1;
        assert!(parse_packet(&bad).is_none());
        let mut bad = pkt.clone();
        bad[5] ^= 1;
        assert!(parse_packet(&bad).is_none());
        assert!(parse_packet(&pkt[..12]).is_none());
        let ack = pack_packet(CONTROL_ACK, 1, 2, 0, &[]);
        assert_eq!(ack.len(), 9);
        assert_eq!(parse_packet(&ack).unwrap().1, b"");
        let mut nine = ack.clone();
        nine[3] = 10;
        nine[8] = checksum(&nine[..8]);
        assert!(parse_packet(&nine).is_none());
    }

    #[test]
    fn sync_and_csm_codecs() {
        let s = LinkSync::proposal();
        let back = LinkSync::parse(&s.pack()).unwrap();
        assert_eq!(back, s);
        assert!(LinkSync::parse(&s.pack()[..9]).is_none());
        let csm = pack_csm(0x1d01, &[(0, b"LEHU\0"), (6, &[0x43, 0x01])]);
        let (id, params) = parse_csm(&csm).unwrap();
        assert_eq!(id, 0x1d01);
        assert_eq!(
            params,
            vec![(0u16, &b"LEHU\0"[..]), (6, &[0x43u8, 0x01][..])]
        );
        let mut bad = csm.clone();
        bad[7] = 0xff;
        assert!(parse_csm(&bad).is_none());
        let mut bad = csm.clone();
        bad[6] = 0;
        bad[7] = 2;
        assert!(parse_csm(&bad).is_none());
        assert!(parse_csm(&csm[..5]).is_none());
        assert!(parse_params(&[0, 5, 0, 1]).is_none());
        assert_eq!(parse_params(&[]).unwrap().len(), 0);
    }

    /// A phone end: bytes we wrote are inspected, bytes queued are read.
    #[derive(Clone, Default)]
    struct Pipe {
        inner: Arc<Mutex<(VecDeque<u8>, Vec<u8>)>>,
    }

    impl Pipe {
        fn feed(&self, bytes: &[u8]) {
            self.inner.lock().unwrap().0.extend(bytes);
        }
        fn written(&self) -> Vec<u8> {
            std::mem::take(&mut self.inner.lock().unwrap().1)
        }
    }

    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut g = self.inner.lock().unwrap();
            if g.0.is_empty() {
                return Err(io::Error::new(io::ErrorKind::WouldBlock, "quiet"));
            }
            let n = buf.len().min(g.0.len());
            for b in buf.iter_mut().take(n) {
                *b = g.0.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.inner.lock().unwrap().1.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Every packet in `bytes`, skipping the detect marker.
    fn packets(bytes: &[u8]) -> Vec<(Header, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off < bytes.len() {
            if bytes[off..].starts_with(&DETECT_MARKER) {
                off += 6;
                continue;
            }
            let len = usize::from(u16::from_be_bytes([bytes[off + 2], bytes[off + 3]]));
            let (h, p) = parse_packet(&bytes[off..off + len]).expect("a whole packet");
            out.push((h, p.to_vec()));
            off += len;
        }
        out
    }

    fn open_link() -> (Link<Pipe>, Pipe) {
        let pipe = Pipe::default();
        let mut link = Link::new(pipe.clone());
        // The phone's SYN with its own numbers, then its ACK of ours.
        let mut peer = LinkSync::proposal();
        peer.max_len = 65535;
        peer.retransmission_timeout_ms = 1000;
        pipe.feed(&pack_packet(CONTROL_SYN, 0x2b, 0, 0, &peer.pack()));
        pipe.feed(&pack_packet(CONTROL_ACK, 0x2b, 99, 0, &[]));
        link.negotiate(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(link.sync().max_len, 65535);
        let sent = packets(&pipe.written());
        assert_eq!(sent[0].0.control, CONTROL_SYN);
        assert_eq!(sent[1].0.control, CONTROL_ACK);
        assert_eq!(sent[1].0.ack, 0x2b);
        (link, pipe)
    }

    #[test]
    fn negotiates_then_acks_data_and_drops_duplicates() {
        let (mut link, pipe) = open_link();
        let csm = pack_csm(0x1d00, &[]);
        let data = pack_packet(CONTROL_ACK, 0x2c, 99, CONTROL_SESSION, &csm);
        pipe.feed(&data);
        pipe.feed(&data); // the phone resends it
        pipe.feed(&pack_packet(CONTROL_ACK, 0x2d, 99, 11, b"other session"));
        let mut out = Vec::new();
        assert!(matches!(
            link.recv_control(&mut out).unwrap(),
            Received::Control
        ));
        assert_eq!(out, csm);
        assert!(matches!(
            link.recv_control(&mut out).unwrap(),
            Received::Nothing
        ));
        let sent = packets(&pipe.written());
        // Three ACKs: the data, its duplicate, the other session's packet.
        assert_eq!(sent.len(), 3);
        assert!(
            sent.iter()
                .all(|(h, p)| h.control == CONTROL_ACK && p.is_empty())
        );
        assert_eq!(sent[0].0.ack, 0x2c);
        assert_eq!(sent[2].0.ack, 0x2d);
    }

    #[test]
    fn file_transfer_datagrams_come_and_go_on_their_own_session() {
        let (mut link, pipe) = open_link();
        assert_eq!(link.file_session(), Some(12));
        pipe.feed(&pack_packet(CONTROL_ACK, 0x2c, 99, 12, &[7, 0x04, 0, 0]));
        let mut out = Vec::new();
        assert!(matches!(
            link.recv_control(&mut out).unwrap(),
            Received::File
        ));
        assert_eq!(out, [7, 0x04, 0, 0]);
        link.send_file(&[7, 0x01]).unwrap();
        let sent = packets(&pipe.written());
        assert_eq!(sent.len(), 2, "the ACK, then the reply");
        assert_eq!(
            (sent[1].0.session, sent[1].1.as_slice()),
            (12, &[7u8, 0x01][..])
        );
        // A link without the session sends none.
        link.sync.sessions.retain(|s| s.kind != FILE_TRANSFER_KIND);
        assert!(link.send_file(&[7, 0x01]).is_err());
    }

    #[test]
    fn retransmits_an_unacknowledged_packet_then_gives_up() {
        let (mut link, pipe) = open_link();
        link.send_control(&pack_csm(0xaa01, &[(0, b"cert")]))
            .unwrap();
        let first = packets(&pipe.written());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].0.seq, 100);
        assert_eq!(first[0].0.session, CONTROL_SESSION);
        // Quiet for longer than the timeout: the packet goes again.
        link.pending.as_mut().unwrap().sent = Instant::now() - Duration::from_secs(2);
        let mut out = Vec::new();
        assert!(matches!(
            link.recv_control(&mut out).unwrap(),
            Received::Nothing
        ));
        assert_eq!(packets(&pipe.written()), first);
        // An ACK clears it.
        pipe.feed(&pack_packet(CONTROL_ACK, 0x2b, 100, 0, &[]));
        assert!(matches!(
            link.recv_control(&mut out).unwrap(),
            Received::Nothing
        ));
        assert!(link.pending.is_none());
        // Never acknowledged: dead after the negotiated tries.
        link.send_control(&pack_csm(0xaa01, &[])).unwrap();
        for _ in 0..4 {
            link.pending.as_mut().unwrap().sent = Instant::now() - Duration::from_secs(2);
            assert!(link.recv_control(&mut out).is_ok());
        }
        link.pending.as_mut().unwrap().sent = Instant::now() - Duration::from_secs(2);
        assert!(link.recv_control(&mut out).is_err());
        assert!(!link.alive());
    }

    #[test]
    fn resyncs_past_noise_and_times_out() {
        let pipe = Pipe::default();
        let mut link = Link::new(pipe.clone());
        pipe.feed(&[0xff, 0x55, 0x02, 0x00, 0xee, 0x10, 0x00, 0xff]);
        pipe.feed(&pack_packet(
            CONTROL_SYN,
            1,
            0,
            0,
            &LinkSync::proposal().pack(),
        ));
        pipe.feed(&[0xff, 0x5a, 0x00, 0x03, 1, 2, 3, 4, 5]); // an implausible length
        pipe.feed(&pack_packet(CONTROL_ACK, 1, 99, 0, &[]));
        assert!(
            link.negotiate(Instant::now() + Duration::from_secs(1))
                .is_ok()
        );
        let mut quiet = Link::new(Pipe::default());
        assert!(
            quiet
                .negotiate(Instant::now() + Duration::from_millis(20))
                .is_err()
        );
        assert!(!quiet.alive());
    }
}
