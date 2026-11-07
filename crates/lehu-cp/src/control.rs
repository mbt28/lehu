//! One control connection from the phone: RTSP in the clear for the
//! pairing, then sealed frames for everything after, routed to the pairing
//! responders, the MFiSAP and the AV session. Bytes in, bytes out, so the
//! transport is any stream; here a TCP socket served on its own thread.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, SocketAddrV6, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use lehu_session::{Fault, Phase};

use subtle::ConstantTimeEq;

use crate::av::{self, AvSession};
use crate::cipher::FrameCipher;
use crate::mfi4;
use crate::pairing::{self, PairSetup, PairVerify};
use crate::rtsp::{self, Request, Response};
use crate::session::Shared;

const PAIRING_CT: &str = "application/pairing+tlv8";
const OCTETS_CT: &str = "application/octet-stream";

/// A connection as the session and the server see it.
pub struct ConnHandle {
    pub id: u32,
    pub controller: Mutex<String>,
    pub verified: AtomicBool,
    /// Asked to end, by the session or by a newer connection.
    pub stop: AtomicBool,
    /// Ended because a newer connection of the phone took over: the
    /// session goes on with that one.
    pub superseded: AtomicBool,
}

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

impl ConnHandle {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            controller: Mutex::new(String::new()),
            verified: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            superseded: AtomicBool::new(false),
        })
    }
}

/// A reconnecting phone supersedes its stale connection at once, by its
/// controller id, rather than leaving it to time out. This head unit
/// shows one phone: another phone's live connection is ended too.
fn supersede(shared: &Shared, newest: &ConnHandle) {
    let mine = newest
        .controller
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    for other in shared
        .connections
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        if other.id == newest.id
            || !other.verified.load(Ordering::Acquire)
            || other.stop.load(Ordering::Acquire)
        {
            continue;
        }
        let theirs = other
            .controller
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if theirs == mine {
            log::info!("carplay: the phone reconnected, its old connection is superseded");
        } else {
            log::info!("carplay: another phone connected, the old connection ends");
        }
        other.superseded.store(true, Ordering::Release);
        other.stop.store(true, Ordering::Release);
    }
}

/// The channel's state: the pairing responders, the cipher once verify is
/// done, and the AV session after it.
pub struct Channel {
    shared: Arc<Shared>,
    handle: Arc<ConnHandle>,
    peer: SocketAddrV6,
    setup: PairSetup,
    verify: PairVerify,
    cipher: Option<FrameCipher>,
    activate_after: bool,
    /// MFi 4 mutual authentication, once the phone asked for it here.
    mfi4: Option<mfi4::Session>,
    pub av: Option<AvSession>,
    sealed_in: Vec<u8>,
    plain_in: Vec<u8>,
}

impl Channel {
    pub fn new(shared: Arc<Shared>, handle: Arc<ConnHandle>, peer: SocketAddrV6) -> Self {
        Self {
            shared,
            handle,
            peer,
            setup: PairSetup::default(),
            verify: PairVerify::default(),
            cipher: None,
            activate_after: false,
            mfi4: None,
            av: None,
            sealed_in: Vec::with_capacity(8192),
            plain_in: Vec::with_capacity(8192),
        }
    }

    fn route(&mut self, req: &Request) -> Response {
        log::debug!(
            "carplay: {} {} ({} bytes){}",
            req.method,
            req.path,
            req.body.len(),
            if self.cipher.is_some() { " sealed" } else { "" }
        );
        match req.path_only() {
            // After an MFi 4 authentication the pair-setup comes through
            // its tunnel.
            "/pair-setup" if self.mfi4.is_some() => self.mfi4_pair_setup(req),
            "/auth-setup"
                if req
                    .header("x-apple-at")
                    .and_then(mfi4::header_number)
                    .is_some_and(|at| at == mfi4::AUTH_TYPE) =>
            {
                self.mfi4_auth_setup(req)
            }
            "/pair-setup" => {
                let body = self.setup.handle(&req.body, &self.shared.store);
                Response::ok().with_body(PAIRING_CT, body)
            }
            "/pair-verify" => {
                let body = self.verify.handle(&req.body, &self.shared.store);
                // The M4 that completes the verify goes in the clear; the
                // cipher takes over right after it.
                if self.verify.verified && self.cipher.is_none() {
                    self.activate_after = true;
                }
                Response::ok().with_body(PAIRING_CT, body)
            }
            "/auth-setup" => {
                let mut chip = self.shared.chip.lock().unwrap_or_else(|e| e.into_inner());
                let Some(chip) = chip.as_deref_mut() else {
                    return Response::status(500);
                };
                match pairing::auth_setup(&req.body, chip) {
                    Some(body) => {
                        log::info!("carplay: /auth-setup answered with {} bytes", body.len());
                        Response::ok().with_body(OCTETS_CT, body)
                    }
                    None => {
                        log::warn!("carplay: /auth-setup failed");
                        Response::status(400)
                    }
                }
            }
            _ => {
                if self.av.is_none()
                    && self.verify.verified
                    && let Some(secret) = self.verify.shared()
                {
                    self.av = Some(AvSession::new(
                        Arc::clone(&self.shared),
                        Arc::clone(&self.handle),
                        *secret,
                        self.peer,
                    ));
                }
                if let Some(av) = self.av.as_mut()
                    && let Some(res) = av.handle(req)
                {
                    return res;
                }
                log::warn!("carplay: unhandled {} {}", req.method, req.path);
                Response::status(404)
            }
        }
    }

    /// An MFi 4 step on `/auth-setup`. Once proven, its tunnel carries
    /// the phone's proof over this connection's pair-verify transcript,
    /// and ours goes back.
    fn mfi4_auth_setup(&mut self, req: &Request) -> Response {
        let dir = self.shared.config.state_dir.join("carplay");
        let session = self.mfi4.get_or_insert_with(|| {
            log::info!("carplay: MFi 4 mutual authentication");
            mfi4::Session::new(&dir)
        });
        if req
            .header("x-apple-pairingtranscript")
            .and_then(mfi4::header_number)
            .is_some_and(|v| v != 0)
        {
            session.binds_pairing = true;
        }
        let verify = &self.verify;
        let mut bound = false;
        let mut check = |shown: &[u8]| {
            let (expected, ours) = verify
                .transcript_proofs()
                .ok_or("nothing to bind, the pair-verify has not run")?;
            let matches = shown
                .get(..expected.len())
                .is_some_and(|s| bool::from(s.ct_eq(&expected)));
            if !matches {
                return Err("the phone's transcript proof does not match".to_string());
            }
            bound = true;
            Ok(ours.to_vec())
        };
        let mut chip = self.shared.chip.lock().unwrap_or_else(|e| e.into_inner());
        let Some(chip) = chip.as_deref_mut() else {
            log::warn!("carplay: MFi 4 asked for and there is no chip");
            return Response::status(500);
        };
        let result = session.handle(&req.body, chip, &mut check);
        if bound {
            log::info!("carplay: the pair-verify is bound to the MFi 4 authentication");
        }
        octets(result)
    }

    /// The pair-setup through the MFi 4 tunnel.
    fn mfi4_pair_setup(&mut self, req: &Request) -> Response {
        let Some(session) = self.mfi4.as_mut() else {
            return Response::status(500);
        };
        let (setup, store) = (&mut self.setup, &self.shared.store);
        let mut through = |data: &[u8]| Ok(setup.handle(data, store));
        let mut chip = self.shared.chip.lock().unwrap_or_else(|e| e.into_inner());
        let Some(chip) = chip.as_deref_mut() else {
            return Response::status(500);
        };
        octets(session.handle(&req.body, chip, &mut through))
    }

    /// Bytes from the phone in, bytes for it out. `None` when the stream
    /// is broken: a frame that fails to open, or bytes that are not RTSP.
    pub fn process(&mut self, incoming: &[u8], out: &mut Vec<u8>) -> Option<()> {
        if let Some(cipher) = self.cipher.as_mut() {
            self.sealed_in.extend_from_slice(incoming);
            if !cipher.decrypt(&mut self.sealed_in, &mut self.plain_in) {
                log::warn!("carplay: control channel decrypt failed");
                return None;
            }
        } else {
            self.plain_in.extend_from_slice(incoming);
        }
        let requests = rtsp::parse(&mut self.plain_in)?;
        for req in &requests {
            let res = self.route(req);
            let bytes = rtsp::build(req, &res);
            match self.cipher.as_mut() {
                Some(c) => c.encrypt_into(&bytes, out),
                None => out.extend_from_slice(&bytes),
            }
            if self.activate_after {
                self.activate_after = false;
                if let Some(keys) = self.verify.keys() {
                    self.cipher = Some(FrameCipher::new(&keys.read, &keys.write));
                    *self
                        .handle
                        .controller
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = self.verify.controller.clone();
                    self.handle.verified.store(true, Ordering::Release);
                    self.shared.control_seen.store(true, Ordering::Release);
                    supersede(&self.shared, &self.handle);
                    log::info!(
                        "carplay: control channel sealed (phone {})",
                        self.verify.controller
                    );
                    self.shared.set_phase(Phase::Identified);
                }
            }
        }
        Some(())
    }
}

fn octets(result: Result<Vec<u8>, String>) -> Response {
    match result {
        Ok(body) => Response::ok().with_body(OCTETS_CT, body),
        Err(_) => Response::status(500),
    }
}

/// Serve one connection until it ends. On the connection's thread.
pub fn serve(
    mut stream: TcpStream,
    peer: SocketAddr,
    shared: Arc<Shared>,
    handle: Arc<ConnHandle>,
) {
    let peer6 = match peer {
        SocketAddr::V6(p) => p,
        SocketAddr::V4(p) => SocketAddrV6::new(p.ip().to_ipv6_mapped(), p.port(), 0, 0),
    };
    log::info!("carplay: control connection from {}", peer6.ip());
    av::keepalive(&stream);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let mut channel = Channel::new(Arc::clone(&shared), Arc::clone(&handle), peer6);
    let mut buf = [0u8; 8192];
    let mut out = Vec::with_capacity(16384);
    let mut fault = Fault::PhoneLeft;
    while shared.is_running() && !handle.stop.load(Ordering::Acquire) {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                out.clear();
                if channel.process(&buf[..n], &mut out).is_none() {
                    fault = Fault::BadMessage;
                    break;
                }
                if !out.is_empty() && stream.write_all(&out).is_err() {
                    fault = Fault::LinkLost;
                    break;
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => {
                log::info!("carplay: control connection: {e}");
                fault = Fault::LinkLost;
                break;
            }
        }
        if let Some(av) = channel.av.as_mut()
            && av.silent()
        {
            log::warn!("carplay: the phone has been silent for 10 s, the session ends");
            fault = Fault::Silent;
            break;
        }
    }
    if let Some(mut av) = channel.av.take() {
        av.stop();
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    log::info!("carplay: control connection closed");
    let was_live = handle.verified.load(Ordering::Acquire);
    let superseded = handle.superseded.load(Ordering::Acquire);
    if shared.closing.load(Ordering::Acquire) {
        // The loop asked for the end: this was the goodbye.
        shared.set_phase(Phase::Closed);
    } else if was_live && !superseded && shared.is_running() {
        shared.fail(fault);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv6Addr, UdpSocket};
    use std::sync::Mutex;
    use std::time::Instant;

    use lehu_core::poll::Wake;
    use lehu_core::queue::Queue;
    use lehu_session::{AudioFormat, AudioKind, Cmd, Event, MicFormat, MicFrame, Ports, Sinks};
    use lehu_video::Codec;

    use crate::Config;
    use crate::av::stream_key;
    use crate::crypto::{self, ChaChaStream, X25519};
    use crate::pairing::test_phone::Client;
    use crate::plist::{self, Value};
    use crate::tlv8;

    #[derive(Default)]
    struct Heard {
        packets: Vec<(AudioKind, AudioFormat, Vec<u8>)>,
        mic: Vec<Option<MicFormat>>,
    }

    struct FakeSinks(Arc<Mutex<Heard>>);

    impl Sinks for FakeSinks {
        fn audio_pcm(&mut self, _: AudioKind, _: u32, _: u8, _: &[i16]) {}
        fn audio_packet(&mut self, kind: AudioKind, format: AudioFormat, payload: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .packets
                .push((kind, format, payload.to_vec()));
        }
        fn mic(&mut self, wanted: Option<MicFormat>) {
            self.0.lock().unwrap().mic.push(wanted);
        }
    }

    /// The phone's end of the control channel: requests framed, sealed
    /// once the verify is done, responses parsed.
    struct Phone {
        channel: Channel,
        cipher: Option<FrameCipher>,
        cseq: u32,
        sealed_in: Vec<u8>,
        plain_in: Vec<u8>,
    }

    impl Phone {
        fn request(&mut self, method: &str, path: &str, ct: &str, body: &[u8]) -> Request {
            self.request_with(method, path, ct, body, &[])
        }

        /// A request with extra header lines.
        fn request_with(
            &mut self,
            method: &str,
            path: &str,
            ct: &str,
            body: &[u8],
            headers: &[&str],
        ) -> Request {
            self.cseq += 1;
            let mut bytes =
                rtsp::build_request(method, path, self.cseq, ct, body).replace_method(method);
            let line = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
            for h in headers {
                let at = line;
                bytes.splice(at..at, format!("{h}\r\n").into_bytes());
            }
            let wire = match self.cipher.as_mut() {
                Some(c) => c.encrypt(&bytes),
                None => bytes,
            };
            let mut out = Vec::new();
            self.channel
                .process(&wire, &mut out)
                .expect("the channel accepts it");
            match self.cipher.as_mut() {
                Some(c) => {
                    self.sealed_in.extend_from_slice(&out);
                    assert!(c.decrypt(&mut self.sealed_in, &mut self.plain_in));
                }
                None => self.plain_in.extend_from_slice(&out),
            }
            let mut responses = rtsp::parse(&mut self.plain_in).expect("a response");
            assert_eq!(responses.len(), 1);
            let r = responses.remove(0);
            assert!(r.is_response(), "{r:?}");
            assert_eq!(r.path, "200", "{r:?}");
            r
        }

        fn plist(&mut self, method: &str, path: &str, body: &Value) -> Value {
            let r = self.request(method, path, crate::info::PLIST_CT, &plist::encode(body));
            if r.body.is_empty() {
                Value::dict()
            } else {
                plist::decode(&r.body).expect("a plist response")
            }
        }
    }

    trait ReplaceMethod {
        fn replace_method(self, m: &str) -> Vec<u8>;
    }
    impl ReplaceMethod for Vec<u8> {
        fn replace_method(self, _: &str) -> Vec<u8> {
            self
        }
    }

    fn drain_events(ports: &Ports) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(e) = ports.events.pop() {
            out.push(e);
        }
        out
    }

    fn wait_for(mut cond: impl FnMut() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(start.elapsed() < Duration::from_secs(5), "timed out");
            thread::sleep(Duration::from_millis(10));
        }
    }

    use std::thread;

    /// An Ultra phone: MFi 4 both ways, the pair-setup through its tunnel,
    /// the pair-verify bound to it, the panels in /info, the vehicle state
    /// enabled and answered on its stream, no second screen.
    #[test]
    fn an_ultra_phone_authenticates_both_ways_and_reads_the_vehicle_state() {
        use crate::mfi4::tests::{FakeChip, Phone as Mfi4Phone};
        use crate::mfi4::wire::{self, Message};
        use crate::{opack, ultra, vehicle};

        lehu_core::log::init(lehu_core::settings::LogLevel::Debug);
        let heard = Arc::new(Mutex::new(Heard::default()));
        let wake = Arc::new(Wake::new().unwrap());
        let ports = Ports::new(wake, Arc::new(Queue::new(8)), Arc::new(Queue::new(8)));
        let mut config = Config::for_test(Codec::H264);
        config.state_dir = config.state_dir.join("ultra");
        config.ultra = true;
        let _ = std::fs::remove_dir_all(&config.state_dir);
        let shared = Shared::for_test(config, Box::new(FakeSinks(heard)), ports.clone());
        *shared.chip.lock().unwrap() = Some(Box::new(FakeChip::new(3)));
        let handle = ConnHandle::new();
        shared.connections.lock().unwrap().push(Arc::clone(&handle));
        let peer = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1, 0, 0);
        let mut phone = Phone {
            channel: Channel::new(Arc::clone(&shared), Arc::clone(&handle), peer),
            cipher: None,
            cseq: 0,
            sealed_in: Vec::new(),
            plain_in: Vec::new(),
        };
        let octets = "application/octet-stream";
        let mfi4_headers = ["X-Apple-AT: 0x20", "X-Apple-PairingTranscript: 1"];
        let mut mfi = Mfi4Phone::new();
        let auth = |phone: &mut Phone, body: &[u8]| {
            phone
                .request_with("POST", "/auth-setup", octets, body, &mfi4_headers)
                .body
        };

        // MFi 4: both sides proven.
        let answer = auth(&mut phone, &Mfi4Phone::setup());
        let start = mfi.start(&answer, false);
        let want = auth(&mut phone, &start);
        assert_eq!(mfi.open(&want).id, 0x5212);
        let cert = mfi.certificate();
        let finish = mfi.open(&auth(&mut phone, &cert));
        assert_eq!(finish.id, 0x5214);
        let ask = mfi.seal(Message::new(0, 0x5112).with(0, finish.param(0).unwrap().to_vec()));
        assert_eq!(mfi.open(&auth(&mut phone, &ask)).id, 0x5213);
        let done = mfi.seal(Message::new(0, 0x51FF));
        assert_eq!(mfi.open(&auth(&mut phone, &done)).id, 0x52FF);

        // The pair-setup's first step through the tunnel: M1 in, M2 out.
        let tlv = "application/pairing+tlv8";
        let write = mfi.tunnel_write(&tlv8::encode(&[(6, &[1])]));
        let taken = phone.request("POST", "/pair-setup", tlv, &write).body;
        assert_eq!(mfi.open(&taken).id, 0x5266);
        let read = mfi.seal(Message::new(0, 0x5163));
        let m2 = phone.request("POST", "/pair-setup", tlv, &read).body;
        let m2 = tlv8::decode(&mfi.tunnel_read(&m2));
        assert_eq!(m2.byte(6), 2);
        assert_eq!(m2.get(3).map(<[u8]>::len), Some(384), "the SRP key");

        // The rest of the pairing as the classic test does it; here the
        // phone's key is stored as M5 would.
        let (phone_seed, phone_ltpk) = crypto::ed25519_generate().unwrap();
        let phone_id = "11:22:33:44:55:77";
        shared.store.save_pairing(phone_id, &phone_ltpk).unwrap();
        let eph = X25519::generate().unwrap();
        let m1 = tlv8::encode(&[(6, &[1]), (3, &eph.public)]);
        let m2_bytes = phone.request("POST", "/pair-verify", tlv, &m1).body;
        let m2 = tlv8::decode(&m2_bytes);
        let acc_eph: [u8; 32] = m2.get(3).unwrap().try_into().unwrap();
        let secret = eph.shared(&acc_eph).unwrap();
        let enc = crypto::hkdf_sha512(
            &secret,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        let sig = crypto::ed25519_sign(
            &phone_seed,
            &[&eph.public[..], phone_id.as_bytes(), &acc_eph].concat(),
        );
        let sub = tlv8::encode(&[(1, phone_id.as_bytes()), (10, &sig)]);
        let sealed = crypto::chacha_seal(&enc, &crypto::nonce_label("PV-Msg03"), &[], &sub);
        let m3 = tlv8::encode(&[(6, &[3]), (5, &sealed)]);
        let m4_bytes = phone.request("POST", "/pair-verify", tlv, &m3).body;
        assert_eq!(tlv8::decode(&m4_bytes).byte(6), 4);
        phone.cipher = Some(FrameCipher::new(
            &crypto::hkdf_sha512(&secret, "Control-Salt", "Control-Read-Encryption-Key"),
            &crypto::hkdf_sha512(&secret, "Control-Salt", "Control-Write-Encryption-Key"),
        ));

        // The binding, sealed now: a wrong proof fails, the right one is
        // answered with ours.
        let transcript = [&m1[..], &m2_bytes, &m3, &m4_bytes].concat();
        let (kt1, kt2) = crate::mfi4::transcript_proofs(&secret, &transcript).unwrap();
        let wrong = auth(&mut phone, &mfi.tunnel_write(&[0; 32]));
        assert_eq!(wire::peek(&wrong).unwrap().1, 0x5265);
        let taken = auth(&mut phone, &mfi.tunnel_write(&kt1));
        assert_eq!(mfi.open(&taken).id, 0x5266);
        let read = mfi.seal(Message::new(0, 0x5163));
        assert_eq!(mfi.tunnel_read(&auth(&mut phone, &read)), kt2);

        // /info: the screen's panel and the instrument panel, the vehicle
        // state; one display.
        let info = phone.plist("GET", "/info", &Value::dict());
        let panels = info.get("displayPanels").and_then(Value::as_array).unwrap();
        assert_eq!(panels.len(), 2);
        assert!(panels[1].get("displayPlugins").is_some());
        assert!(info.get(vehicle::INFO_KEY).is_some());
        assert_eq!(
            info.get("displays")
                .and_then(Value::as_array)
                .map(<[_]>::len),
            Some(1)
        );

        // SETUP: the vehicle state taken, the UI sync not (no theme).
        let offered = Value::Array(vec![
            Value::str("vehicleStateProtocol"),
            Value::str("uiSync"),
            Value::str("somethingElse"),
        ]);
        let setup = phone.plist(
            "SETUP",
            "rtsp://x/y",
            &Value::dict()
                .with("timingPort", Value::Int(0))
                .with("features", offered),
        );
        let enabled: Vec<&str> = setup
            .get("enabledFeatures")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(enabled.contains(&"vehicleStateProtocol"), "{enabled:?}");
        assert!(!enabled.contains(&"uiSync"), "{enabled:?}");

        // The vehicle state's stream, and an alternate screen refused.
        let streams = phone.plist(
            "SETUP",
            "rtsp://x/y",
            &Value::dict().with(
                "streams",
                Value::Array(vec![
                    Value::dict()
                        .with("type", Value::Int(130))
                        .with("seed", Value::Int(4242))
                        .with(
                            "clientTypeUUID",
                            Value::str(&vehicle::DATASTREAM_UUID.to_lowercase()),
                        ),
                    Value::dict()
                        .with("type", Value::Int(111))
                        .with("streamConnectionID", Value::Int(9)),
                ]),
            ),
        );
        let streams = streams.get("streams").and_then(Value::as_array).unwrap();
        assert_eq!(streams.len(), 1, "no second screen");
        assert_eq!(streams[0].int_or("streamID", 0), 2);
        let port = streams[0].int_or("dataPort", 0) as u16;
        let mut data = TcpStream::connect((Ipv6Addr::LOCALHOST, port)).unwrap();
        let mut cipher = FrameCipher::new(
            &stream_key(&secret, 4242, "DataStream-Input-Encryption-Key"),
            &stream_key(&secret, 4242, "DataStream-Output-Encryption-Key"),
        );
        let request = opack::encode(&opack::dict([
            ("pluginID", opack::Value::Int(0)),
            (
                "pluginData",
                opack::Value::Data(opack::encode(&opack::dict([
                    ("command", opack::Value::str("configRequest")),
                    ("transactionID", opack::Value::Int(1)),
                ]))),
            ),
        ]));
        let sync = ultra::package(0x7379_6e63, 0x636f_6d6d, 5, &request);
        data.write_all(&cipher.encrypt(&sync)).unwrap();
        data.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (mut sealed, mut plain, mut got) = (Vec::new(), Vec::new(), Vec::new());
        let mut buf = [0u8; 4096];
        while got.len() < 2 {
            let n = data.read(&mut buf).unwrap();
            assert!(n > 0, "the stream closed");
            sealed.extend_from_slice(&buf[..n]);
            assert!(cipher.decrypt(&mut sealed, &mut plain));
            while let Ok(Some(p)) = ultra::next_package(&mut plain) {
                got.push(p);
            }
        }
        assert_eq!(got[0].kind, 0x7270_6c79, "the sync package acknowledged");
        assert_eq!(got[0].reply_token, 5);
        let outer = opack::decode(&got[1].body).unwrap();
        let answer = opack::decode(
            outer
                .get("pluginData")
                .and_then(opack::Value::as_data)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            answer.get("command").and_then(opack::Value::as_str),
            Some("configResponse")
        );
        if let Some(mut av) = phone.channel.av.take() {
            av.stop();
        }
        let _ = std::fs::remove_dir_all(&shared.config.state_dir);
    }

    #[test]
    fn a_phone_pairs_sets_up_and_streams() {
        lehu_core::log::init(lehu_core::settings::LogLevel::Debug);
        let heard = Arc::new(Mutex::new(Heard::default()));
        let wake = Arc::new(Wake::new().unwrap());
        let video = Arc::new(Queue::new(8));
        let recycle = Arc::new(Queue::new(8));
        let ports = Ports::new(wake, video, recycle);
        let mut config = Config::for_test(Codec::H264);
        config.state_dir = config.state_dir.join("control");
        let _ = std::fs::remove_dir_all(&config.state_dir);
        let shared = Shared::for_test(
            config,
            Box::new(FakeSinks(Arc::clone(&heard))),
            ports.clone(),
        );
        let handle = ConnHandle::new();
        shared.connections.lock().unwrap().push(Arc::clone(&handle));
        let peer = SocketAddrV6::new(Ipv6Addr::LOCALHOST, 1, 0, 0);
        let mut phone = Phone {
            channel: Channel::new(Arc::clone(&shared), Arc::clone(&handle), peer),
            cipher: None,
            cseq: 0,
            sealed_in: Vec::new(),
            plain_in: Vec::new(),
        };
        let tlv = "application/pairing+tlv8";

        // Pair-setup: SRP, then the long-term keys.
        let m2 = tlv8::decode(
            &phone
                .request("POST", "/pair-setup", tlv, &tlv8::encode(&[(6, &[1])]))
                .body,
        );
        let client = Client::new();
        let (key, m1) = client.proof(m2.get(2).unwrap(), m2.get(3).unwrap());
        let m4 = tlv8::decode(
            &phone
                .request(
                    "POST",
                    "/pair-setup",
                    tlv,
                    &tlv8::encode(&[(6, &[3]), (3, &client.big_a), (4, &m1)]),
                )
                .body,
        );
        assert_eq!(m4.byte(6), 4);
        let (phone_seed, phone_ltpk) = crypto::ed25519_generate().unwrap();
        let phone_id = "11:22:33:44:55:66";
        let enc = crypto::hkdf_sha512(&key, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info");
        let sign = crypto::hkdf_sha512(
            &key,
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
        );
        let sig = crypto::ed25519_sign(
            &phone_seed,
            &[&sign[..], phone_id.as_bytes(), &phone_ltpk].concat(),
        );
        let sub = tlv8::encode(&[(1, phone_id.as_bytes()), (3, &phone_ltpk), (10, &sig)]);
        let sealed = crypto::chacha_seal(&enc, &crypto::nonce_label("PS-Msg05"), &[], &sub);
        let m6 = tlv8::decode(
            &phone
                .request(
                    "POST",
                    "/pair-setup",
                    tlv,
                    &tlv8::encode(&[(6, &[5]), (5, &sealed)]),
                )
                .body,
        );
        assert_eq!(m6.byte(6), 6);

        // Pair-verify; its M4 comes in the clear, everything after sealed.
        let eph = X25519::generate().unwrap();
        let m2 = tlv8::decode(
            &phone
                .request(
                    "POST",
                    "/pair-verify",
                    tlv,
                    &tlv8::encode(&[(6, &[1]), (3, &eph.public)]),
                )
                .body,
        );
        let acc_eph: [u8; 32] = m2.get(3).unwrap().try_into().unwrap();
        let secret = eph.shared(&acc_eph).unwrap();
        let enc = crypto::hkdf_sha512(
            &secret,
            "Pair-Verify-Encrypt-Salt",
            "Pair-Verify-Encrypt-Info",
        );
        let sig = crypto::ed25519_sign(
            &phone_seed,
            &[&eph.public[..], phone_id.as_bytes(), &acc_eph].concat(),
        );
        let sub = tlv8::encode(&[(1, phone_id.as_bytes()), (10, &sig)]);
        let sealed = crypto::chacha_seal(&enc, &crypto::nonce_label("PV-Msg03"), &[], &sub);
        let m4 = tlv8::decode(
            &phone
                .request(
                    "POST",
                    "/pair-verify",
                    tlv,
                    &tlv8::encode(&[(6, &[3]), (5, &sealed)]),
                )
                .body,
        );
        assert_eq!(m4.byte(6), 4);
        assert_eq!(m4.get(7), None);
        phone.cipher = Some(FrameCipher::new(
            &crypto::hkdf_sha512(&secret, "Control-Salt", "Control-Read-Encryption-Key"),
            &crypto::hkdf_sha512(&secret, "Control-Salt", "Control-Write-Encryption-Key"),
        ));
        assert!(handle.verified.load(Ordering::Acquire));
        assert_eq!(drain_events(&ports), vec![Event::Phase(Phase::Identified)]);

        // The capabilities, sealed.
        let info = phone.plist("GET", "/info", &Value::dict());
        assert_eq!(info.str_or("name", ""), "LEHU");

        // The session SETUP, then a screen and a telephony stream.
        let setup = phone.plist(
            "SETUP",
            "rtsp://x/y",
            &Value::dict()
                .with("name", Value::str("Test iPhone"))
                .with("timingPort", Value::Int(0))
                .with("keepAliveLowPower", Value::Bool(true)),
        );
        assert!(setup.int_or("eventPort", 0) > 0);
        assert!(setup.int_or("timingPort", 0) > 0);
        assert!(setup.int_or("keepAlivePort", 0) > 0);
        assert_eq!(
            shared.phone_name.get().map(String::as_str),
            Some("Test iPhone")
        );
        let screen_id: i64 = -5;
        let streams = phone.plist(
            "SETUP",
            "rtsp://x/y",
            &Value::dict().with(
                "streams",
                Value::Array(vec![
                    Value::dict()
                        .with("type", Value::Int(110))
                        .with("streamConnectionID", Value::Int(screen_id)),
                    Value::dict()
                        .with("type", Value::Int(100))
                        .with("streamConnectionID", Value::Int(77))
                        .with("audioType", Value::str("telephony"))
                        .with("audioFormat", Value::Int(0x10))
                        .with("input", Value::Bool(true)),
                ]),
            ),
        );
        let streams = streams.get("streams").and_then(Value::as_array).unwrap();
        let screen_port = streams[0].int_or("dataPort", 0) as u16;
        let audio_port = streams[1].int_or("dataPort", 0) as u16;
        assert!(screen_port > 0 && audio_port > 0);
        let events = drain_events(&ports);
        assert!(events.contains(&Event::Call(true)), "{events:?}");
        assert_eq!(
            heard
                .lock()
                .unwrap()
                .mic
                .last()
                .copied()
                .flatten()
                .map(|m| m.frame_tenths_ms),
            Some(75)
        );
        phone.plist("RECORD", "rtsp://x/y", &Value::dict());
        // The event channel: our commands go out on it once connected.
        let _event =
            TcpStream::connect((Ipv6Addr::LOCALHOST, setup.int_or("eventPort", 0) as u16)).unwrap();

        // Telephony audio down: one RTP packet, sealed with the output key,
        // reaches the sink with its samples in native order.
        let audio_key = stream_key(&secret, 77, "DataStream-Output-Encryption-Key");
        let phone_socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
        let mut rtp = vec![0x80u8, 100, 0, 1, 0, 0, 0, 10, 0, 0, 0, 0];
        let mut body = vec![0x12u8, 0x34, 0x56, 0x78];
        let tag = ChaChaStream::new(&audio_key)
            .seal(&crypto::nonce64(9), &rtp[4..12], &mut body)
            .unwrap();
        rtp.extend_from_slice(&body);
        rtp.extend_from_slice(&tag);
        rtp.extend_from_slice(&9u64.to_le_bytes());
        phone_socket
            .send_to(&rtp, (Ipv6Addr::LOCALHOST, audio_port))
            .unwrap();
        wait_for(|| !heard.lock().unwrap().packets.is_empty());
        {
            let h = heard.lock().unwrap();
            let (kind, format, payload) = &h.packets[0];
            assert_eq!(*kind, AudioKind::Call);
            assert_eq!(format.rate, 16000);
            assert_eq!(format.channels, 1);
            assert_eq!(payload, &[0x34, 0x12, 0x78, 0x56]);
        }
        let events = drain_events(&ports);
        assert!(
            events.contains(&Event::Audio(AudioKind::Call, true)),
            "{events:?}"
        );

        // The screen: a configuration in the clear, then a sealed frame
        // (the stream id is negative: the key renders it unsigned).
        let mut screen = TcpStream::connect((Ipv6Addr::LOCALHOST, screen_port)).unwrap();
        let sps = [0x67u8, 0x42, 0, 0x1e];
        let pps = [0x68u8, 0xce, 0x38, 0x80];
        let mut avcc = vec![1u8, 0x42, 0, 0x1e, 0xff, 0xe1, 0, 4];
        avcc.extend_from_slice(&sps);
        avcc.extend_from_slice(&[1, 0, 4]);
        avcc.extend_from_slice(&pps);
        let mut msg = vec![0u8; 128];
        msg[..4].copy_from_slice(&(avcc.len() as u32).to_le_bytes());
        msg[4] = 1;
        msg.extend_from_slice(&avcc);
        let frame_plain = [0u8, 0, 0, 3, 0x65, 0x88, 0x84];
        let mut frame = frame_plain.to_vec();
        let mut hdr = vec![0u8; 128];
        hdr[..4].copy_from_slice(&((frame.len() + 16) as u32).to_le_bytes());
        let out_key = stream_key(&secret, screen_id, "DataStream-Output-Encryption-Key");
        let tag = ChaChaStream::new(&out_key)
            .seal(&crypto::nonce64(0), &hdr, &mut frame)
            .unwrap();
        msg.extend_from_slice(&hdr);
        msg.extend_from_slice(&frame);
        msg.extend_from_slice(&tag);
        screen.write_all(&msg).unwrap();
        wait_for(|| ports.video.len() >= 2);
        let config = ports.video.pop().unwrap();
        assert!(config.config);
        assert_eq!(&config.data()[..4], &[0, 0, 0, 1]);
        let unit = ports.video.pop().unwrap();
        assert_eq!(unit.data(), &[0, 0, 0, 1, 0x65, 0x88, 0x84]);
        let events = drain_events(&ports);
        assert!(
            events.contains(&Event::VideoCodec(Codec::H264)),
            "{events:?}"
        );
        assert!(events.contains(&Event::Phase(Phase::Ready)));
        assert!(events.contains(&Event::Phase(Phase::Live)));

        // A keyframe request replays the parameter sets first.
        shared.cmds.push_discard(Cmd::Keyframe);
        shared.cmd_wake.wake();
        wait_for(|| !ports.video.is_empty());
        assert!(ports.video.pop().unwrap().config);

        // /feedback lists the live stream with the sample it should be at.
        let fb = phone.plist("POST", "/feedback", &Value::dict());
        let listed = fb.get("streams").and_then(Value::as_array).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].int_or("streamConnectionID", 0), 77);
        assert_eq!(listed[0].int_or("sampleRate", 0), 16000);
        assert!(listed[0].get("sampleTime").is_some());
        assert!(listed[0].get("timestamp").is_some());

        // The microphone goes back to where the audio came from, sealed
        // with the input key: the frames come coded as the phone asked
        // (the sink makes CarPlay's LPCM big-endian) and go out as they are.
        let mut frame = MicFrame::from_slice(&[0x01, 0x02, 0x03, 0x04]).unwrap();
        frame.len = 4;
        ports.mic.push_discard(frame);
        let mut got = [0u8; 256];
        phone_socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let (n, _) = phone_socket.recv_from(&mut got).unwrap();
        assert_eq!(n, 12 + 4 + 16 + 8);
        assert_eq!(got[1], 100);
        let in_key = stream_key(&secret, 77, "DataStream-Input-Encryption-Key");
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&got[n - 8..n]);
        let aad = got[4..12].to_vec();
        let mut sealed = got[12..n - 8].to_vec();
        assert!(ChaChaStream::new(&in_key).open(&nonce, &aad, &mut sealed));
        assert_eq!(&sealed[..4], &[0x01, 0x02, 0x03, 0x04]);

        // The id-matched TEARDOWN ends the call and the microphone.
        phone.plist(
            "TEARDOWN",
            "rtsp://x/y",
            &Value::dict().with(
                "streams",
                Value::Array(vec![
                    Value::dict()
                        .with("type", Value::Int(100))
                        .with("streamConnectionID", Value::Int(77)),
                ]),
            ),
        );
        let events = drain_events(&ports);
        assert!(events.contains(&Event::Call(false)), "{events:?}");
        assert_eq!(heard.lock().unwrap().mic.last().copied().flatten(), None);
        let fb = phone.plist("POST", "/feedback", &Value::dict());
        assert_eq!(
            fb.get("streams").and_then(Value::as_array).unwrap().len(),
            0
        );

        // The dock's car icon asks for our UI.
        phone.plist(
            "POST",
            "/command",
            &Value::dict().with("type", Value::str("requestUI")),
        );
        assert_eq!(drain_events(&ports), vec![Event::HostUi]);

        // A sealed frame that does not open ends the channel.
        let mut out = Vec::new();
        assert!(
            phone
                .channel
                .process(
                    &[
                        5, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19,
                        20, 21, 22
                    ],
                    &mut out
                )
                .is_none()
        );
        drop(phone);
        let _ = std::fs::remove_dir_all(&shared.config.state_dir);
    }
}
