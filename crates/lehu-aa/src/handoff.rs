// SPDX-License-Identifier: GPL-3.0-or-later
//! The Bluetooth handoff: one accepted RFCOMM connection, the wireless
//! setup conversation on it, and the credentials handed over. The
//! conversation is a small state machine driven with the clock passed in,
//! so the tests run it without a socket or a timer; `run` drives it over a
//! connected stream until the phone hangs up or the session ends.
//!
//! The order FastCarPlay sent and phones accepted: our version request
//! first; the phone's version response brings the start request with our
//! endpoint; the phone's info request brings the credentials. A phone that
//! asks for the network before answering the version (LIVI saw such) gets
//! the endpoint and then the credentials. After the handoff the link stays
//! open as long as the phone keeps it: it reports how the join went and
//! pings; closing it from our side would end the setup on the phone.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lehu_bt::RfcommProfile;
use lehu_core::clock::now_ms;

use crate::aaw::{self, Network};

/// The record the C++ registered for the service, verbatim: the UUID, L2CAP
/// then RFCOMM on channel 8, the public browse group and the name. Some
/// bluetoothd versions publish nothing for a custom UUID given only a role
/// and a channel, and the phone's SDP query then finds no service.
pub const SDP_RECORD: &str = concat!(
    "<?xml version=\"1.0\" encoding=\"UTF-8\" ?>",
    "<record>",
    "  <attribute id=\"0x0001\"><sequence>",
    "    <uuid value=\"4de17a00-52cb-11e6-bdf4-0800200c9a66\" />",
    "  </sequence></attribute>",
    "  <attribute id=\"0x0004\"><sequence>",
    "    <sequence><uuid value=\"0x0100\" /></sequence>",
    "    <sequence><uuid value=\"0x0003\" /><uint8 value=\"0x08\" /></sequence>",
    "  </sequence></attribute>",
    "  <attribute id=\"0x0005\"><sequence>",
    "    <uuid value=\"0x1002\" />",
    "  </sequence></attribute>",
    "  <attribute id=\"0x0100\"><text value=\"Android Auto Wireless\" /></attribute>",
    "</record>",
);

/// The profile the bootstrap registers with the Bluetooth manager.
pub fn profile() -> RfcommProfile {
    let mut p = RfcommProfile::server(aaw::UUID, aaw::SERVICE_NAME, aaw::CHANNEL);
    p.record = Some(SDP_RECORD.to_string());
    p
}

/// How long the phone has to answer the version request, and then to ask
/// for the network. A phone that connected the service and says nothing
/// is not setting up; its connection is closed so a retry can start clean.
pub const VERSION_WAIT_MS: u64 = 10_000;
pub const INFO_WAIT_MS: u64 = 30_000;

/// How long a read on the stream handed to `run` should wait when the
/// phone is quiet: `run` looks at the clock and the stop flag between reads.
pub const READ_WAIT: Duration = Duration::from_millis(200);

/// Where the conversation stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Our version request is out.
    Version,
    /// The endpoint is out; the phone has yet to ask for the network.
    Start,
    /// The credentials are out: the phone is joining, or has joined.
    Done,
}

/// Why the conversation ended on our side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// The phone said nothing in time at this step.
    Timeout(Step),
    /// A body that cannot be this protocol's.
    Bad(aaw::BadFrame),
}

impl std::fmt::Display for Ended {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ended::Timeout(Step::Version) => f.write_str("no version response"),
            Ended::Timeout(Step::Start) => f.write_str("the phone never asked for the network"),
            Ended::Timeout(Step::Done) => f.write_str("timed out"),
            Ended::Bad(b) => write!(f, "{b}"),
        }
    }
}

/// The phone, as its version response named it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    /// The instance id, the registry's key.
    pub id: String,
    pub serial: String,
    pub name: String,
}

/// One handoff. `out` on every call collects the frames to send, in order.
pub struct Handoff<'a> {
    network: &'a Network,
    step: Step,
    /// When the step times out; 0 for never.
    deadline: u64,
    identity: Identity,
    /// The phone's last join report, when it made one.
    join: Option<i32>,
    body: Vec<u8>,
}

impl<'a> Handoff<'a> {
    /// Begin: the version request goes into `out`.
    pub fn begin(network: &'a Network, now: u64, out: &mut Vec<u8>) -> Self {
        let mut h = Self {
            network,
            step: Step::Version,
            deadline: now + VERSION_WAIT_MS,
            identity: Identity::default(),
            join: None,
            body: Vec::with_capacity(aaw::MAX_BODY),
        };
        h.push(out, aaw::MSG_VERSION_REQUEST, &network.version_request());
        h
    }

    pub fn step(&self) -> Step {
        self.step
    }

    /// The credentials were handed over.
    pub fn handed_off(&self) -> bool {
        self.step == Step::Done
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// The phone's join report: `Some(0)` joined, a negative failed.
    pub fn join_status(&self) -> Option<i32> {
        self.join
    }

    /// When `on_tick` next has something to do, in milliseconds from `now`.
    pub fn next_due(&self, now: u64) -> u64 {
        if self.deadline == 0 {
            u64::MAX
        } else {
            self.deadline.saturating_sub(now)
        }
    }

    fn push(&mut self, out: &mut Vec<u8>, id: u16, m: &impl prost::Message) {
        // The three bodies are bounded by the credentials' limits; a body
        // that does not fit is a programming error, not a wire condition.
        if aaw::encode_frame(&mut self.body, id, m).is_ok() {
            out.extend_from_slice(&self.body);
        }
    }

    fn send_start(&mut self, now: u64, out: &mut Vec<u8>) {
        self.push(out, aaw::MSG_START_REQUEST, &self.network.start_request());
        self.step = Step::Start;
        self.deadline = now + INFO_WAIT_MS;
    }

    /// A frame from the phone; the answers, if any, are appended to `out`.
    pub fn on_frame(&mut self, now: u64, id: u16, body: &[u8], out: &mut Vec<u8>) {
        match id {
            aaw::MSG_VERSION_RESPONSE => {
                if let Some(r) = aaw::decode::<aaw::WifiVersionResponse>(body) {
                    let who = r.mobile_device_identity.unwrap_or_default();
                    self.identity = Identity {
                        id: who.mobile_device_id.unwrap_or_default(),
                        serial: r.device_serial.unwrap_or_default(),
                        name: who.bluetooth_name.unwrap_or_default(),
                    };
                    match r.status {
                        Some(s) if s != 0 => log::warn!(
                            "aa-bt: the phone answers the version with {} ({s})",
                            aaw::status_name(s)
                        ),
                        _ => {}
                    }
                    log::info!(
                        "aa-bt: phone {} (serial {}, version {}.{})",
                        if self.identity.id.is_empty() {
                            "-"
                        } else {
                            &self.identity.id
                        },
                        if self.identity.serial.is_empty() {
                            "-"
                        } else {
                            &self.identity.serial
                        },
                        r.major_version.unwrap_or(0),
                        r.minor_version.unwrap_or(0)
                    );
                } else {
                    log::warn!("aa-bt: a version response that does not decode");
                }
                if self.step == Step::Version {
                    self.send_start(now, out);
                }
            }
            aaw::MSG_INFO_REQUEST => {
                // The endpoint goes first if the phone skipped the version.
                if self.step == Step::Version {
                    self.send_start(now, out);
                }
                log::info!(
                    "aa-bt: the phone asks for the network: {} on channel {}, connect to {}:{}",
                    self.network.ssid,
                    self.network.channel,
                    self.network.ip,
                    self.network.port
                );
                self.push(out, aaw::MSG_INFO_RESPONSE, &self.network.info_response());
                self.step = Step::Done;
                self.deadline = 0;
            }
            aaw::MSG_CONNECTION_STATUS => {
                let s = aaw::decode::<aaw::WifiConnectionStatus>(body)
                    .and_then(|c| c.status)
                    .unwrap_or(0);
                self.join = Some(s);
                if s == 0 {
                    log::info!("aa-bt: the phone joined {}", self.network.ssid);
                } else {
                    log::warn!(
                        "aa-bt: the phone could not join {}: {} ({s})",
                        self.network.ssid,
                        aaw::status_name(s)
                    );
                }
            }
            aaw::MSG_START_RESPONSE => {
                let s = aaw::decode::<aaw::WifiStartResponse>(body)
                    .and_then(|c| c.status)
                    .unwrap_or(0);
                if s != 0 {
                    log::warn!(
                        "aa-bt: the phone answers the endpoint with {} ({s})",
                        aaw::status_name(s)
                    );
                }
            }
            aaw::MSG_PING => {
                // Echoed as it came, which keeps the link alive.
                if aaw::frame_into(&mut self.body, aaw::MSG_PONG, body).is_ok() {
                    out.extend_from_slice(&self.body);
                }
            }
            other => log::debug!("aa-bt: message {other} ({} bytes) ignored", body.len()),
        }
    }

    /// The clock moved: the step's deadline.
    pub fn on_tick(&mut self, now: u64) -> Result<(), Ended> {
        if self.deadline != 0 && now >= self.deadline {
            return Err(Ended::Timeout(self.step));
        }
        Ok(())
    }
}

/// What `run` ends with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub handed_off: bool,
    pub identity: Identity,
    pub join: Option<i32>,
}

/// A quiet read: the stream's timeout, however it spells it.
fn quiet(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}

/// Fill `dst` exactly from `stream`, ticking `h` between quiet reads.
/// `Ok(false)` when the phone hung up or the session ended.
fn read_exact<S: Read>(
    stream: &mut S,
    dst: &mut [u8],
    h: &mut Handoff<'_>,
    running: &AtomicBool,
) -> Result<bool, Ended> {
    let mut got = 0;
    while got < dst.len() {
        if !running.load(Ordering::Acquire) {
            return Ok(false);
        }
        h.on_tick(now_ms())?;
        match stream.read(&mut dst[got..]) {
            Ok(0) => return Ok(false),
            Ok(n) => got += n,
            Err(e) if quiet(&e) => {}
            Err(e) => {
                log::debug!("aa-bt: read: {e}");
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// The handoff over a connected stream, whose reads return within about
/// `READ_WAIT` when quiet (the RFCOMM stream's timeout, a Unix socket's).
/// Runs until the phone hangs up, a step times out or `running` clears.
pub fn run<S: Read + Write>(mut stream: S, network: &Network, running: &AtomicBool) -> Outcome {
    let mut out = Vec::with_capacity(2 * (aaw::HEADER_LEN + aaw::MAX_BODY));
    let mut h = Handoff::begin(network, now_ms(), &mut out);
    let mut head = [0u8; aaw::HEADER_LEN];
    let mut body = Vec::with_capacity(aaw::MAX_BODY);
    let ended = loop {
        if !out.is_empty() {
            if let Err(e) = stream.write_all(&out) {
                log::info!("aa-bt: write: {e}");
                break None;
            }
            out.clear();
        }
        let hd = match read_exact(&mut stream, &mut head, &mut h, running) {
            Ok(true) => match aaw::parse_head(head) {
                Ok(hd) => hd,
                Err(b) => break Some(Ended::Bad(b)),
            },
            Ok(false) => break None,
            Err(e) => break Some(e),
        };
        body.clear();
        body.resize(usize::from(hd.len), 0);
        if !body.is_empty() {
            match read_exact(&mut stream, &mut body, &mut h, running) {
                Ok(true) => {}
                Ok(false) => break None,
                Err(e) => break Some(e),
            }
        }
        h.on_frame(now_ms(), hd.id, &body, &mut out);
    };
    match ended {
        Some(e) => log::warn!("aa-bt: the handoff ends: {e}"),
        None => log::info!(
            "aa-bt: the Bluetooth link ended (credentials {})",
            if h.handed_off() { "sent" } else { "not sent" }
        ),
    }
    Outcome {
        handed_off: h.handed_off(),
        identity: h.identity().clone(),
        join: h.join_status(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::thread;

    use prost::Message;

    fn network() -> Network {
        Network {
            ssid: "LEHU".into(),
            passphrase: "lehu1234".into(),
            bssid: "aa:bb:cc:dd:ee:ff".into(),
            channel: 6,
            ip: "192.168.53.1".into(),
            port: 5277,
        }
    }

    /// The frames in `bytes`, as (id, body).
    fn frames(bytes: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut out = Vec::new();
        let mut off = 0;
        while off + 4 <= bytes.len() {
            let h = aaw::parse_head([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
                .unwrap();
            let body = &bytes[off + 4..off + 4 + usize::from(h.len)];
            out.push((h.id, body.to_vec()));
            off += 4 + usize::from(h.len);
        }
        assert_eq!(off, bytes.len(), "a torn frame");
        out
    }

    fn version_response(id: &str, serial: &str) -> Vec<u8> {
        aaw::WifiVersionResponse {
            major_version: Some(6),
            minor_version: Some(0),
            device_serial: Some(serial.into()),
            status: Some(0),
            mobile_device_identity: Some(aaw::MobileDeviceIdentity {
                mobile_device_id: Some(id.into()),
                bluetooth_name: Some("Pixel".into()),
                ..Default::default()
            }),
        }
        .encode_to_vec()
    }

    #[test]
    fn the_record_and_profile_are_the_ones_that_worked() {
        let p = profile();
        assert_eq!(p.uuid, "4de17a00-52cb-11e6-bdf4-0800200c9a66");
        assert_eq!(p.channel, 8);
        assert_eq!(p.name, "Android Auto Wireless");
        assert_eq!(p.role, lehu_bt::Role::Server);
        let record = p.record.unwrap();
        assert_eq!(record, SDP_RECORD);
        assert!(record.contains("<uuid value=\"4de17a00-52cb-11e6-bdf4-0800200c9a66\" />"));
        assert!(record.contains("<uuid value=\"0x0003\" /><uint8 value=\"0x08\" />"));
        assert!(record.contains("<uuid value=\"0x1002\" />"));
        assert!(record.contains("<text value=\"Android Auto Wireless\" />"));
        assert!(
            !record.contains("0x0009"),
            "the C++ record carries no profile descriptor"
        );
    }

    #[test]
    fn the_conversation_in_the_order_phones_take() {
        let n = network();
        let mut out = Vec::new();
        let mut h = Handoff::begin(&n, 1000, &mut out);
        assert_eq!(h.step(), Step::Version);
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, aaw::MSG_VERSION_REQUEST);
        assert_eq!(sent[0].1, n.version_request().encode_to_vec());
        out.clear();
        // Quiet within the wait is fine.
        assert_eq!(h.on_tick(1000 + VERSION_WAIT_MS - 1), Ok(()));
        assert_eq!(h.next_due(1000), VERSION_WAIT_MS);
        h.on_frame(
            2000,
            aaw::MSG_VERSION_RESPONSE,
            &version_response("abc", "SN1"),
            &mut out,
        );
        assert_eq!(h.step(), Step::Start);
        assert_eq!(
            h.identity(),
            &Identity {
                id: "abc".into(),
                serial: "SN1".into(),
                name: "Pixel".into(),
            }
        );
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, aaw::MSG_START_REQUEST);
        assert_eq!(sent[0].1, n.start_request().encode_to_vec());
        out.clear();
        // The start response says nothing new; a ping is echoed.
        h.on_frame(2100, aaw::MSG_START_RESPONSE, &[0x18, 0x00], &mut out);
        assert!(out.is_empty());
        h.on_frame(2200, aaw::MSG_PING, &[0x08, 0x05], &mut out);
        assert_eq!(frames(&out), [(aaw::MSG_PONG, vec![0x08, 0x05])]);
        out.clear();
        // The info wait runs from the start request.
        assert_eq!(h.next_due(2000), INFO_WAIT_MS);
        assert_eq!(h.on_tick(2000 + INFO_WAIT_MS - 1), Ok(()));
        h.on_frame(3000, aaw::MSG_INFO_REQUEST, &[], &mut out);
        assert_eq!(h.step(), Step::Done);
        assert!(h.handed_off());
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, aaw::MSG_INFO_RESPONSE);
        assert_eq!(sent[0].1, n.info_response().encode_to_vec());
        out.clear();
        // No more deadlines: the phone keeps the link as long as it likes.
        assert_eq!(h.next_due(3000), u64::MAX);
        assert_eq!(h.on_tick(3000 + 3_600_000), Ok(()));
        h.on_frame(
            4000,
            aaw::MSG_CONNECTION_STATUS,
            &aaw::WifiConnectionStatus {
                status: Some(0),
                error_message: None,
            }
            .encode_to_vec(),
            &mut out,
        );
        assert_eq!(h.join_status(), Some(0));
        h.on_frame(
            4100,
            aaw::MSG_CONNECTION_STATUS,
            &aaw::WifiConnectionStatus {
                status: Some(-3),
                error_message: Some("bad key".into()),
            }
            .encode_to_vec(),
            &mut out,
        );
        assert_eq!(h.join_status(), Some(-3));
        // Unknown messages and a second version response change nothing.
        h.on_frame(4200, 77, &[1, 2, 3], &mut out);
        h.on_frame(
            4300,
            aaw::MSG_VERSION_RESPONSE,
            &version_response("abc", "SN1"),
            &mut out,
        );
        assert!(out.is_empty());
        assert_eq!(h.step(), Step::Done);
    }

    #[test]
    fn a_phone_that_asks_first_gets_the_endpoint_then_the_network() {
        let n = network();
        let mut out = Vec::new();
        let mut h = Handoff::begin(&n, 1000, &mut out);
        out.clear();
        h.on_frame(1500, aaw::MSG_INFO_REQUEST, &[], &mut out);
        let ids: Vec<u16> = frames(&out).iter().map(|f| f.0).collect();
        assert_eq!(ids, [aaw::MSG_START_REQUEST, aaw::MSG_INFO_RESPONSE]);
        assert!(h.handed_off());
        // A version response after the fact is noted, not answered again.
        out.clear();
        h.on_frame(
            1600,
            aaw::MSG_VERSION_RESPONSE,
            &version_response("x", "y"),
            &mut out,
        );
        assert!(out.is_empty());
        assert_eq!(h.identity().id, "x");
    }

    #[test]
    fn silence_ends_each_step_in_its_time() {
        let n = network();
        let mut out = Vec::new();
        let mut h = Handoff::begin(&n, 1000, &mut out);
        assert_eq!(
            h.on_tick(1000 + VERSION_WAIT_MS),
            Err(Ended::Timeout(Step::Version))
        );
        let mut h = Handoff::begin(&n, 1000, &mut out);
        h.on_frame(5000, aaw::MSG_VERSION_RESPONSE, &[], &mut out);
        assert_eq!(h.step(), Step::Start);
        assert_eq!(h.on_tick(5000 + INFO_WAIT_MS - 1), Ok(()));
        assert_eq!(
            h.on_tick(5000 + INFO_WAIT_MS),
            Err(Ended::Timeout(Step::Start))
        );
        assert_eq!(
            Ended::Timeout(Step::Start).to_string(),
            "the phone never asked for the network"
        );
        assert_eq!(
            Ended::Bad(aaw::BadFrame::TooLong(2000)).to_string(),
            "a body of 2000 bytes announced"
        );
    }

    /// The scripted phone on the other end of a socket pair.
    fn scripted_phone(mut s: UnixStream, hang_up: bool) -> thread::JoinHandle<Vec<(u16, Vec<u8>)>> {
        thread::spawn(move || {
            let mut seen = Vec::new();
            let read_frame = |s: &mut UnixStream| {
                let mut head = [0u8; 4];
                s.read_exact(&mut head).unwrap();
                let h = aaw::parse_head(head).unwrap();
                let mut body = vec![0u8; usize::from(h.len)];
                s.read_exact(&mut body).unwrap();
                (h.id, body)
            };
            seen.push(read_frame(&mut s));
            s.write_all(&{
                let mut f = Vec::new();
                aaw::frame_into(
                    &mut f,
                    aaw::MSG_VERSION_RESPONSE,
                    &version_response("abc", "SN1"),
                )
                .unwrap();
                f
            })
            .unwrap();
            seen.push(read_frame(&mut s));
            let mut f = Vec::new();
            aaw::frame_into(&mut f, aaw::MSG_START_RESPONSE, &[0x18, 0x00]).unwrap();
            s.write_all(&f).unwrap();
            aaw::frame_into(&mut f, aaw::MSG_INFO_REQUEST, &[]).unwrap();
            s.write_all(&f).unwrap();
            seen.push(read_frame(&mut s));
            aaw::frame_into(&mut f, aaw::MSG_PING, &[0x08, 0x2a]).unwrap();
            s.write_all(&f).unwrap();
            seen.push(read_frame(&mut s));
            aaw::frame_into(
                &mut f,
                aaw::MSG_CONNECTION_STATUS,
                &aaw::WifiConnectionStatus {
                    status: Some(0),
                    error_message: None,
                }
                .encode_to_vec(),
            )
            .unwrap();
            s.write_all(&f).unwrap();
            if hang_up {
                drop(s);
            } else {
                // Keep the link until the session ends under us.
                let mut b = [0u8; 16];
                let _ = s.read(&mut b);
            }
            seen
        })
    }

    #[test]
    fn the_handoff_runs_over_a_socket_until_the_phone_hangs_up() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let phone = scripted_phone(theirs, true);
        let running = AtomicBool::new(true);
        let outcome = run(ours, &network(), &running);
        assert!(outcome.handed_off);
        assert_eq!(outcome.identity.id, "abc");
        assert_eq!(outcome.identity.serial, "SN1");
        assert_eq!(outcome.join, Some(0));
        let seen = phone.join().unwrap();
        let ids: Vec<u16> = seen.iter().map(|f| f.0).collect();
        assert_eq!(
            ids,
            [
                aaw::MSG_VERSION_REQUEST,
                aaw::MSG_START_REQUEST,
                aaw::MSG_INFO_RESPONSE,
                aaw::MSG_PONG
            ]
        );
        assert_eq!(seen[3].1, [0x08, 0x2a]);
        assert_eq!(seen[2].1, network().info_response().encode_to_vec());
    }

    #[test]
    fn the_session_ending_ends_the_handoff_and_a_bad_head_too() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let phone = scripted_phone(theirs, false);
        let running = Arc::new(AtomicBool::new(true));
        let r = Arc::clone(&running);
        let runner = thread::spawn(move || run(ours, &network(), &r));
        thread::sleep(Duration::from_millis(100));
        running.store(false, Ordering::Release);
        let outcome = runner.join().unwrap();
        assert!(outcome.handed_off);
        phone.join().unwrap();

        // A head announcing more than the protocol carries ends the link.
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        ours.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let running = AtomicBool::new(true);
        let phone = thread::spawn(move || {
            let mut head = [0u8; 4];
            theirs.read_exact(&mut head).unwrap();
            let h = aaw::parse_head(head).unwrap();
            let mut body = vec![0u8; usize::from(h.len)];
            theirs.read_exact(&mut body).unwrap();
            theirs.write_all(&[0xff, 0xff, 0, 5]).unwrap();
            let mut b = [0u8; 4];
            // Our end closes.
            assert_eq!(theirs.read(&mut b).unwrap(), 0);
        });
        let outcome = run(ours, &network(), &running);
        assert!(!outcome.handed_off);
        phone.join().unwrap();
    }
}
