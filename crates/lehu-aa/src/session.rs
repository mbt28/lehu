// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Mehmet Tekbas
//! The running session: two threads around the machine and the transport,
//! and for a wireless phone the thread that waits for it first.
//!
//! - `aa-process` reads frames from the transport (which blocks on the
//!   USB read thread's ring), reassembles and decrypts them in the
//!   machine, and pushes the video units it gets back into the decoder's
//!   queue with back-pressure (a depth of 4, 500 ms), outside the lock.
//! - `aa-write` takes what the outbox holds in order: the machine's
//!   messages, encrypted and fragmented here and written with the
//!   transport's retry; the loop's commands, translated by the machine;
//!   and the close. Between items it ticks the machine's clock (pings,
//!   the timeouts, the goodbye wait) and, while the phone listens, sends
//!   the microphone frames the capture thread put in `ports.mic`.
//!
//! The two meet only in the machine's mutex, held briefly: never across a
//! USB write or a wait on the decoder.
//!
//! Over USB the link is up when the session starts. Over Wi-Fi the session
//! starts waiting (`wireless`): the phone comes over Bluetooth for the
//! handoff, joins the access point and connects to our TCP port, and only
//! then do the two threads start, over that connection.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use lehu_core::clock::now_ms;
use lehu_core::queue::Queue;
use lehu_session::{AccessUnit, Cmd, Kind, Link, MicFrame, Phase, Ports, Sinks};

use crate::aaw;
use crate::consts::*;
use crate::frame::{Class, Fragments, Header, parse_total};
use crate::handoff;
use crate::machine::{self, Config, Machine, OUTBOX_DEPTH, Work, now_ns, phase_from_u8};
use crate::tcp;
use crate::tls::Tls;
use crate::transport::{Transport, UsbTransport};
use crate::wireless;

/// The decoder's queue: wait this deep, this long, then drop the oldest.
const VIDEO_DEPTH: usize = 4;
const VIDEO_WAIT: Duration = Duration::from_millis(500);
/// The write thread looks at the clock at least this often.
const IDLE_WAIT_MS: u64 = 1000;
/// With the microphone open, how long it waits between looks at the
/// frames the capture thread put in the queue (a frame is 20 ms).
const MIC_WAIT_MS: u64 = 10;

#[derive(Debug)]
pub enum Error {
    Usb(crate::transport::Error),
    Tls(crate::tls::Error),
    Thread(std::io::Error),
    /// The system's access point is not up: nothing to hand a phone.
    NoAccessPoint,
    /// The Bluetooth manager refused the profile (it is not running).
    Bluetooth(std::io::Error),
    /// The TCP port the phones are told could not be bound.
    Bind(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usb(e) => write!(f, "usb: {e}"),
            Error::Tls(e) => write!(f, "tls: {e}"),
            Error::Thread(e) => write!(f, "thread: {e}"),
            Error::NoAccessPoint => f.write_str("the access point is not up"),
            Error::Bluetooth(e) => write!(f, "bluetooth: {e}"),
            Error::Bind(e) => write!(f, "tcp port {}: {e}", TCP_PORT),
        }
    }
}

impl std::error::Error for Error {}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The GAL session over a link that is up: the machine and its two
/// threads. Started at once over USB; over Wi-Fi when the phone connects.
pub(crate) struct Gal {
    outbox: Arc<Queue<Work>>,
    machine: Arc<Mutex<Machine>>,
    transport: Arc<dyn Transport>,
    process: Option<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
}

impl Gal {
    /// The version request goes out at once; `phase`, `name` and
    /// `identity` are the session's mirrors the machine fills.
    pub(crate) fn start(
        transport: Arc<dyn Transport>,
        config: Config,
        sinks: Box<dyn Sinks>,
        ports: Ports,
        phase: Arc<AtomicU8>,
        name: Arc<OnceLock<String>>,
        identity: Arc<OnceLock<String>>,
    ) -> Result<Gal, Error> {
        let tls = Tls::new(config.force_chacha20).map_err(Error::Tls)?;
        let outbox = Arc::new(Queue::new(OUTBOX_DEPTH));
        let mut m = Machine::new(
            config,
            tls,
            Arc::clone(&outbox),
            ports.clone(),
            sinks,
            phase,
            name,
            identity,
            now_ms(),
        );
        m.start();
        let machine = Arc::new(Mutex::new(m));
        let process = {
            let transport = Arc::clone(&transport);
            let machine = Arc::clone(&machine);
            let video = Arc::clone(&ports.video);
            let outbox = Arc::clone(&outbox);
            thread::Builder::new()
                .name("aa-process".into())
                .spawn(move || process_loop(&*transport, &machine, &video, &outbox))
                .map_err(Error::Thread)?
        };
        let writer = {
            let transport = Arc::clone(&transport);
            let machine = Arc::clone(&machine);
            let outbox = Arc::clone(&outbox);
            let mic = Arc::clone(&ports.mic);
            thread::Builder::new()
                .name("aa-write".into())
                .spawn(move || write_loop(&*transport, &machine, &outbox, &mic))
                .map_err(Error::Thread)?
        };
        log::info!("aa: session over {}", transport.name());
        Ok(Gal {
            outbox,
            machine,
            transport,
            process: Some(process),
            writer: Some(writer),
        })
    }

    /// Tear the link down under the threads: the process thread reports
    /// the loss and Closed, the write thread ends. Nothing is said to the
    /// phone.
    pub(crate) fn abort(&self) {
        self.transport.close();
    }

    fn ending(&self) -> bool {
        lock(&self.machine).ending()
    }

    fn cipher_name(&self) -> &'static str {
        lock(&self.machine).cipher_name()
    }

    fn join(&mut self) {
        if let Some(t) = self.writer.take() {
            let _ = t.join();
        }
        if let Some(t) = self.process.take() {
            let _ = t.join();
        }
    }
}

pub struct Session {
    link: Link,
    phase: Arc<AtomicU8>,
    name: Arc<OnceLock<String>>,
    identity: Arc<OnceLock<String>>,
    /// The last phone handed the access point over Bluetooth.
    bt_peer: Arc<Mutex<String>>,
    /// The GAL session: there from the start over USB; over Wi-Fi, from
    /// the phone's connection on (the wireless thread puts it here).
    gal: Arc<Mutex<Option<Gal>>>,
    /// The wireless thread, while the session is a wireless one.
    waiter: Option<wireless::Waiter>,
}

impl Session {
    /// A session over a phone already in accessory mode (after
    /// `switch_to_accessory` and its re-enumeration).
    pub fn start(
        config: Config,
        device: &lehu_usb::Device,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<Session, Error> {
        let transport = UsbTransport::open(device, &config.usb).map_err(Error::Usb)?;
        Self::over(Arc::new(transport), Link::Usb, config, sinks, ports)
    }

    /// A session over any transport that is up: the accessory's bulk pipe,
    /// a pipe in tests.
    pub fn over(
        transport: Arc<dyn Transport>,
        link: Link,
        config: Config,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<Session, Error> {
        let phase = Arc::new(AtomicU8::new(Phase::Spawned as u8));
        let name = Arc::new(OnceLock::new());
        let identity = Arc::new(OnceLock::new());
        let gal = Gal::start(
            transport,
            config,
            sinks,
            ports,
            Arc::clone(&phase),
            Arc::clone(&name),
            Arc::clone(&identity),
        )?;
        Ok(Session {
            link,
            phase,
            name,
            identity,
            bt_peer: Arc::new(Mutex::new(String::new())),
            gal: Arc::new(Mutex::new(Some(gal))),
            waiter: None,
        })
    }

    /// The session for the phones that come over Bluetooth and the access
    /// point: the Android Auto service is registered with the Bluetooth
    /// manager (now, or when bluetoothd comes up), the TCP port bound, and
    /// every Bluetooth connection gets the handoff that tells the phone
    /// the endpoint and hands it `ap`'s credentials; the phone joins the
    /// access point, connects, and the GAL session runs over that
    /// connection as it does over USB. The session sits in Spawned until
    /// then, for as long as the loop keeps it. It ends through its link
    /// (the TCP connection breaking, the pings unanswered) or the access
    /// point going down under it, with `Fault::LinkLost`; it does not
    /// start over, the loop does. `ap` must be up: there is nothing to
    /// hand a phone otherwise.
    pub fn start_wireless(
        config: Config,
        bt: &lehu_bt::Manager,
        ap: lehu_wifi::Params,
        sinks: Box<dyn Sinks>,
        ports: Ports,
    ) -> Result<Session, Error> {
        Self::start_wireless_on(config, bt, ap, sinks, ports, TCP_PORT)
    }

    /// `start_wireless` on `port` (0 for any, in tests).
    pub(crate) fn start_wireless_on(
        config: Config,
        bt: &lehu_bt::Manager,
        ap: lehu_wifi::Params,
        sinks: Box<dyn Sinks>,
        ports: Ports,
        port: u16,
    ) -> Result<Session, Error> {
        if !ap.valid() {
            return Err(Error::NoAccessPoint);
        }
        let listener = tcp::Listener::bind(port).map_err(Error::Bind)?;
        let rfcomm = bt
            .register_rfcomm(handoff::profile())
            .map_err(Error::Bluetooth)?;
        let network = aaw::Network {
            ssid: ap.ssid,
            passphrase: ap.passphrase,
            bssid: ap.bssid,
            channel: ap.channel,
            ip: ap.ip,
            port: listener.port(),
        };
        let phase = Arc::new(AtomicU8::new(Phase::Spawned as u8));
        let name = Arc::new(OnceLock::new());
        let bt_peer = Arc::new(Mutex::new(String::new()));
        let identity = Arc::new(OnceLock::new());
        let gal = Arc::new(Mutex::new(None));
        log::info!(
            "aa: wireless session on port {}, waiting for a phone over Bluetooth ({} on {})",
            network.port,
            network.ssid,
            ap.iface
        );
        let waiter = wireless::spawn(wireless::Start {
            rfcomm,
            listener,
            network,
            iface: ap.iface,
            config,
            sinks,
            ports,
            gal: Arc::clone(&gal),
            phase: Arc::clone(&phase),
            name: Arc::clone(&name),
            identity: Arc::clone(&identity),
            bt_peer: Arc::clone(&bt_peer),
        })
        .map_err(Error::Thread)?;
        Ok(Session {
            link: Link::Wifi,
            phase,
            name,
            identity,
            bt_peer,
            gal,
            waiter: Some(waiter),
        })
    }

    /// The phone's instance id from its discovery request, the registry's
    /// key; `None` until Identified, or when the phone sent none.
    pub fn phone_identity(&self) -> Option<String> {
        self.identity.get().cloned()
    }

    /// The negotiated cipher, for the status line; "none" before the
    /// phone connected.
    pub fn cipher_name(&self) -> &'static str {
        lock(&self.gal).as_ref().map_or("none", Gal::cipher_name)
    }

    /// The TCP port a wireless session's phones are told; `None` over USB.
    pub fn wireless_port(&self) -> Option<u16> {
        self.waiter.as_ref().map(wireless::Waiter::port)
    }
}

impl lehu_session::Session for Session {
    fn kind(&self) -> Kind {
        Kind::AndroidAuto
    }

    fn link(&self) -> Link {
        self.link
    }

    fn phase(&self) -> Phase {
        phase_from_u8(self.phase.load(Ordering::Acquire))
    }

    fn phone(&self) -> Option<&str> {
        self.name.get().map(String::as_str)
    }

    /// The phone's instance id, the registry's key.
    fn identity(&self) -> Option<String> {
        self.phone_identity()
    }

    fn bt_address(&self) -> Option<String> {
        let peer = self.bt_peer.lock().unwrap_or_else(|e| e.into_inner());
        (!peer.is_empty()).then(|| peer.clone())
    }

    /// To the GAL session when there is one. Before a wireless phone
    /// connected only Close means anything: the wait ends.
    fn command(&self, cmd: Cmd) {
        if let Some(g) = &*lock(&self.gal) {
            if !g.outbox.push_discard(Work::Cmd(cmd)) {
                log::warn!("aa: outbox full, {cmd:?} dropped");
            }
            return;
        }
        match (&self.waiter, cmd) {
            (Some(w), Cmd::Close) => w.end(),
            _ => log::debug!("aa: {cmd:?} before a phone connected, dropped"),
        }
    }

    /// Goodbye, then the threads joined: the write thread ends once the
    /// phone answered the ByeBye or a second passed, the process thread
    /// when the link is closed under it. A wireless session still waiting
    /// just stops waiting.
    fn stop(mut self: Box<Self>) {
        let gal = lock(&self.gal).take();
        if let Some(mut g) = gal {
            if !g.ending() {
                g.outbox.push_drop_oldest(Work::Cmd(Cmd::Close));
            }
            g.join();
        }
        if let Some(mut w) = self.waiter.take() {
            w.stop();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // The abrupt end: nothing is said to the phone.
        if let Some(w) = &self.waiter {
            w.end();
        }
        let gal = lock(&self.gal).take();
        if let Some(mut g) = gal {
            g.abort();
            g.outbox.close();
            g.join();
        }
        self.waiter = None;
    }
}

fn process_loop(
    transport: &dyn Transport,
    machine: &Mutex<Machine>,
    video: &Queue<AccessUnit>,
    outbox: &Queue<Work>,
) {
    let mut head = [0u8; Header::LEN];
    let mut ext = [0u8; 4];
    let mut payload: Vec<u8> = Vec::with_capacity(MAX_FRAME_PAYLOAD);
    let fault = loop {
        if !transport.read_exact(&mut head) {
            break Some(lehu_session::Fault::LinkLost);
        }
        let header = match Header::parse(head) {
            Ok(h) => h,
            Err(d) => {
                log::warn!("aa: {d}, closing the link");
                break Some(lehu_session::Fault::LinkLost);
            }
        };
        let total = if header.has_total() {
            if !transport.read_exact(&mut ext) {
                break Some(lehu_session::Fault::LinkLost);
            }
            match parse_total(ext) {
                Ok(t) => t,
                Err(d) => {
                    log::warn!("aa: {d}, closing the link");
                    break Some(lehu_session::Fault::LinkLost);
                }
            }
        } else {
            0
        };
        payload.clear();
        payload.resize(usize::from(header.len), 0);
        if !payload.is_empty() && !transport.read_exact(&mut payload) {
            break Some(lehu_session::Fault::LinkLost);
        }
        let unit = match lock(machine).on_frame(now_ms(), &header, total, &payload) {
            Ok(u) => u,
            Err(e) => {
                log::warn!("aa: {e}, closing the link");
                break Some(e.fault());
            }
        };
        if let Some(u) = unit
            && !video.push_wait_drop_oldest(u, VIDEO_DEPTH, VIDEO_WAIT)
        {
            log::debug!("aa: a video unit was dropped");
        }
    };
    transport.close();
    lock(machine).finish(fault);
    // The write thread may sleep until its next tick; this wakes it, and
    // it ends on the link being down.
    outbox.push_drop_oldest(Work::Close);
    log::info!("aa: session over");
}

struct Writer<'a> {
    transport: &'a dyn Transport,
    machine: &'a Mutex<Machine>,
    plain: Vec<u8>,
    cipher: Vec<u8>,
    frame: Vec<u8>,
}

impl Writer<'_> {
    /// Message id and body, encrypted as the class says, fragmented,
    /// written. False when the link is gone.
    fn send(&mut self, channel: u8, class: Class, msg_id: u16, head: &[u8], body: &[u8]) -> bool {
        self.plain.clear();
        self.plain.extend_from_slice(&msg_id.to_be_bytes());
        self.plain.extend_from_slice(head);
        self.plain.extend_from_slice(body);
        if class.encrypted() {
            self.cipher.clear();
            if let Err(e) = lock(self.machine).encrypt(&self.plain, &mut self.cipher) {
                log::warn!("aa: {e}");
                return false;
            }
        }
        let payload = if class.encrypted() {
            &self.cipher
        } else {
            &self.plain
        };
        let mut fragments = Fragments::new(channel, class, payload);
        while fragments.next_into(&mut self.frame) {
            log::trace!(
                "aa: tx ch {channel} id {msg_id:#06x} wire {} bytes",
                self.frame.len()
            );
            if !self.transport.write_all(&self.frame) {
                return false;
            }
        }
        true
    }
}

fn write_loop(
    transport: &dyn Transport,
    machine: &Mutex<Machine>,
    outbox: &Queue<Work>,
    mic: &Queue<MicFrame>,
) {
    let mut w = Writer {
        transport,
        machine,
        plain: Vec::with_capacity(machine::OUT_BODY + 2),
        cipher: Vec::with_capacity(machine::OUT_BODY + 64),
        frame: Vec::with_capacity(MAX_FRAME_PAYLOAD + 8),
    };
    let mut mic_was_open = false;
    let mut closed = false;
    while transport.connected() {
        let now = now_ms();
        let (due, mic_open) = {
            let mut m = lock(machine);
            m.on_tick(now, now_ns());
            (m.next_due(now), m.mic_open())
        };
        if mic_open {
            while let Some(f) = mic.pop() {
                let ts = now_ns().to_be_bytes();
                if !w.send(CH_MIC, Class::EncSignal, MSG_MEDIA_DATA, &ts, f.as_slice()) {
                    break;
                }
            }
        } else if mic_was_open {
            // Frames captured after the phone stopped listening.
            mic.clear();
        }
        mic_was_open = mic_open;
        let wait = due.min(if mic_open { MIC_WAIT_MS } else { IDLE_WAIT_MS });
        let Some(work) = outbox.pop_wait(Duration::from_millis(wait)) else {
            continue;
        };
        match work {
            Work::Close => {
                closed = true;
                break;
            }
            Work::Cmd(mut cmd) => {
                // Queued moves collapse into the newest when the write
                // thread fell behind: only the last position matters.
                while machine::touch_is_move_only(&cmd)
                    && outbox.peek(
                        |next| matches!(next, Some(Work::Cmd(n)) if machine::touch_is_move_only(n)),
                    )
                {
                    if let Some(Work::Cmd(next)) = outbox.pop() {
                        cmd = next;
                    }
                }
                lock(machine).on_cmd(now_ms(), now_ns(), cmd);
            }
            Work::Msg(m) => {
                if !w.send(m.channel, m.class, m.msg_id, &[], m.body()) {
                    break;
                }
            }
        }
    }
    transport.close();
    if closed {
        log::info!("aa: link closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::sync::Mutex;

    use lehu_core::poll::Wake;
    use lehu_session::{Event, Fault, Session as _};

    use crate::machine::tests::{FakeSinks, Sunk, config};
    use crate::proto;
    use crate::tls::test_phone;

    /// The phone's end of the link: frames in and out, TLS once up. The
    /// stream reads with a timeout the caller set.
    struct Phone<S: Read + Write> {
        stream: S,
        tls: Tls,
        plain: Vec<u8>,
    }

    impl<S: Read + Write> Phone<S> {
        fn new(stream: S) -> Self {
            Self {
                stream,
                tls: test_phone(None),
                plain: Vec::new(),
            }
        }

        /// The next whole message: (channel, id, body), decrypted.
        fn read(&mut self) -> Option<(u8, u16, Vec<u8>)> {
            self.plain.clear();
            loop {
                let mut head = [0u8; 4];
                self.stream.read_exact(&mut head).ok()?;
                let h = Header::parse(head).unwrap();
                if h.has_total() {
                    let mut ext = [0u8; 4];
                    self.stream.read_exact(&mut ext).ok()?;
                }
                let mut payload = vec![0u8; usize::from(h.len)];
                self.stream.read_exact(&mut payload).ok()?;
                if h.encrypted() {
                    self.tls.decrypt(&payload, &mut self.plain).unwrap();
                } else {
                    self.plain.extend_from_slice(&payload);
                }
                if h.last() {
                    let id = u16::from_be_bytes([self.plain[0], self.plain[1]]);
                    // The head unit's own pings come whenever the clock
                    // says; they are not what a test waits for.
                    if h.channel == CH_CONTROL && id == MSG_PING_REQUEST {
                        self.plain.clear();
                        continue;
                    }
                    return Some((h.channel, id, self.plain[2..].to_vec()));
                }
            }
        }

        fn send(&mut self, ch: u8, class: Class, id: u16, body: &[u8]) {
            let mut plain = id.to_be_bytes().to_vec();
            plain.extend_from_slice(body);
            let mut wire = Vec::new();
            if class.encrypted() {
                self.tls.encrypt(&plain, &mut wire).unwrap();
            } else {
                wire = plain;
            }
            let mut f = Fragments::new(ch, class, &wire);
            let mut frame = Vec::new();
            while f.next_into(&mut frame) {
                self.stream.write_all(&frame).unwrap();
            }
        }

        fn send_proto(&mut self, ch: u8, id: u16, m: &impl prost::Message) {
            let mut buf = [0u8; machine::OUT_BODY];
            let n = proto::encode_into(m, &mut buf).unwrap();
            self.send(ch, Class::EncSignal, id, &buf[..n]);
        }

        /// Version and TLS, to the auth complete.
        fn connect(&mut self) {
            assert_eq!(
                self.read().unwrap(),
                (CH_CONTROL, MSG_VERSION_REQUEST, vec![0, 1, 0, 7])
            );
            self.send(
                CH_CONTROL,
                Class::Plaintext,
                MSG_VERSION_RESPONSE,
                &[0, 1, 0, 7, 0, 0],
            );
            loop {
                let (ch, id, body) = self.read().unwrap();
                assert_eq!(ch, CH_CONTROL);
                if id == MSG_AUTH_COMPLETE {
                    break;
                }
                assert_eq!(id, MSG_SSL_HANDSHAKE);
                let mut reply = Vec::new();
                self.tls.accept_step(&body, &mut reply).unwrap();
                if !reply.is_empty() {
                    self.send(CH_CONTROL, Class::Plaintext, MSG_SSL_HANDSHAKE, &reply);
                }
            }
            assert!(self.tls.ready());
        }
    }

    struct Rig {
        session: Box<Session>,
        phone: Phone<UnixStream>,
        ports: Ports,
        sunk: Arc<Mutex<Vec<Sunk>>>,
    }

    fn rig() -> Rig {
        let (ours, theirs) = UnixStream::pair().unwrap();
        theirs
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let wake = Arc::new(Wake::new().unwrap());
        let video = Arc::new(Queue::new(8));
        let recycle = Arc::new(Queue::new(8));
        let ports = Ports::new(wake, video, recycle);
        let sunk = Arc::new(Mutex::new(Vec::new()));
        let transport = Arc::new(crate::transport::StreamTransport::new(ours, "pair"));
        let session = Session::over(
            transport,
            Link::Usb,
            config(),
            Box::new(FakeSinks(Arc::clone(&sunk))),
            ports.clone(),
        )
        .unwrap();
        Rig {
            session: Box::new(session),
            phone: Phone::new(theirs),
            ports,
            sunk,
        }
    }

    fn events(ports: &Ports) -> Vec<Event> {
        let mut out = Vec::new();
        while let Some(e) = ports.events.pop() {
            out.push(e);
        }
        out
    }

    /// Wait up to 5 s for the phase, draining the events.
    fn wait_for(ports: &Ports, got: &mut Vec<Event>, phase: Phase) {
        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            got.extend(events(ports));
            if got.contains(&Event::Phase(phase)) {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("never reached {phase:?}: {got:?}");
    }

    #[test]
    fn a_version_mismatch_ends_the_session_with_a_fault() {
        let mut r = rig();
        assert_eq!(r.session.kind(), Kind::AndroidAuto);
        assert_eq!(r.session.link(), Link::Usb);
        assert_eq!(r.session.phase(), Phase::Spawned);
        assert_eq!(r.session.phone(), None);
        assert_eq!(r.phone.read().unwrap().1, MSG_VERSION_REQUEST);
        r.phone.send(
            CH_CONTROL,
            Class::Plaintext,
            MSG_VERSION_RESPONSE,
            &[0, 2, 0, 0, 0, 1],
        );
        let mut got = Vec::new();
        wait_for(&r.ports, &mut got, Phase::Closed);
        assert_eq!(
            got,
            [Event::Fault(Fault::Rejected), Event::Phase(Phase::Closed)]
        );
        assert_eq!(r.session.phase(), Phase::Closed);
        assert!(
            r.phone.read().is_none(),
            "the link is closed under the phone"
        );
        let start = std::time::Instant::now();
        r.session.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn the_handshake_runs_over_the_threads_and_stop_says_goodbye() {
        let mut r = rig();
        r.phone.connect();
        assert_eq!(r.session.cipher_name(), r.phone.tls.cipher_name());
        r.phone.send_proto(
            CH_CONTROL,
            MSG_SERVICE_DISCOVERY_REQUEST,
            &proto::ServiceDiscoveryRequest {
                device_name: Some("Pixel".into()),
                phone_info: None,
            },
        );
        let (ch, id, body) = r.phone.read().unwrap();
        assert_eq!((ch, id), (CH_CONTROL, MSG_SERVICE_DISCOVERY_RESPONSE));
        assert!(proto::decode::<proto::ServiceDiscoveryResponse>(&body).is_some());
        let mut got = Vec::new();
        wait_for(&r.ports, &mut got, Phase::Identified);
        assert_eq!(r.session.phone(), Some("Pixel"));
        assert_eq!(r.session.phase(), Phase::Identified);
        // A touch before the input channel is open goes nowhere.
        r.session.command(Cmd::Touch {
            count: 1,
            contacts: [lehu_input::Contact::default(); lehu_input::MAX_SLOTS],
        });
        // The phone pings; the answer is plaintext.
        r.phone.send_proto(
            CH_CONTROL,
            MSG_PING_REQUEST,
            &proto::PingRequest { timestamp: 5 },
        );
        assert_eq!(
            r.phone.read().unwrap(),
            (CH_CONTROL, MSG_PING_RESPONSE, vec![0x08, 0x05])
        );
        // The graceful end: ByeBye, the phone's answer, the join.
        let Rig {
            session,
            mut phone,
            ports,
            ..
        } = r;
        let stopper = thread::spawn(move || {
            let start = std::time::Instant::now();
            session.stop();
            start.elapsed()
        });
        let (ch, id, _) = phone.read().unwrap();
        assert_eq!((ch, id), (CH_CONTROL, MSG_BYEBYE_REQUEST));
        phone.send(CH_CONTROL, Class::EncSignal, MSG_BYEBYE_RESPONSE, &[]);
        let took = stopper.join().unwrap();
        assert!(
            took < Duration::from_millis(900),
            "did not wait for the timeout: {took:?}"
        );
        wait_for(&ports, &mut got, Phase::Closed);
        assert!(got.contains(&Event::Phase(Phase::Closing)));
        assert!(!got.iter().any(|e| matches!(e, Event::Fault(_))), "{got:?}");
    }

    #[test]
    fn microphone_frames_flow_to_the_phone_while_it_listens() {
        let mut r = rig();
        r.phone.connect();
        r.phone.send_proto(
            CH_CONTROL,
            MSG_SERVICE_DISCOVERY_REQUEST,
            &proto::ServiceDiscoveryRequest {
                device_name: None,
                phone_info: None,
            },
        );
        r.phone.read().unwrap();
        r.phone.send_proto(
            CH_MIC,
            MSG_CHANNEL_OPEN_REQUEST,
            &proto::ChannelOpenRequest {
                priority: 0,
                service_id: 9,
            },
        );
        assert_eq!(r.phone.read().unwrap().1, MSG_CHANNEL_OPEN_RESPONSE);
        r.phone.send_proto(
            CH_MIC,
            MSG_MICROPHONE_REQUEST,
            &proto::MicrophoneRequest { open: true },
        );
        assert_eq!(r.phone.read().unwrap().1, MSG_MICROPHONE_RESPONSE);
        assert_eq!(r.phone.read().unwrap().1, MSG_MEDIA_START);
        let frame = MicFrame::from_slice(&[0x11; 640]).unwrap();
        r.ports.mic.push_discard(frame);
        r.ports.mic.push_discard(frame);
        for _ in 0..2 {
            let (ch, id, body) = r.phone.read().unwrap();
            assert_eq!((ch, id), (CH_MIC, MSG_MEDIA_DATA));
            assert_eq!(body.len(), 8 + 640);
            assert!(body[8..].iter().all(|&b| b == 0x11));
        }
        assert!(
            r.sunk
                .lock()
                .unwrap()
                .iter()
                .any(|s| matches!(s, Sunk::Mic(Some(_))))
        );
        r.phone.send(CH_MIC, Class::EncSignal, MSG_MEDIA_STOP, &[]);
        // The abrupt end: nothing is said, the phone just loses the link.
        drop(r.session);
        assert!(r.phone.read().is_none());
        assert_eq!(r.sunk.lock().unwrap().last(), Some(&Sunk::Mic(None)));
    }

    fn access_point() -> lehu_wifi::Params {
        lehu_wifi::Params {
            iface: "no-such-interface".into(),
            ssid: "LEHU".into(),
            passphrase: "lehu1234".into(),
            channel: 6,
            ip: "127.0.0.1".into(),
            bssid: "aa:bb:cc:dd:ee:ff".into(),
        }
    }

    /// A wireless session waiting on a port of its own, with the manager
    /// it registered its profile with (no bus on the host: the
    /// registration waits for one).
    fn wireless() -> (Box<Session>, Ports, lehu_bt::Manager) {
        let bt = lehu_bt::Manager::start(&Default::default(), |_| {}).unwrap();
        let wake = Arc::new(Wake::new().unwrap());
        let video = Arc::new(Queue::new(8));
        let recycle = Arc::new(Queue::new(8));
        let ports = Ports::new(wake, video, recycle);
        let sunk = Arc::new(Mutex::new(Vec::new()));
        let session = Session::start_wireless_on(
            config(),
            &bt,
            access_point(),
            Box::new(FakeSinks(sunk)),
            ports.clone(),
            0,
        )
        .unwrap();
        (Box::new(session), ports, bt)
    }

    fn connect_phone(port: u16) -> Phone<std::net::TcpStream> {
        let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Phone::new(stream)
    }

    #[test]
    fn a_wireless_session_waits_then_runs_the_phone_over_tcp_until_its_link_breaks() {
        let (session, ports, _bt) = wireless();
        assert_eq!(session.kind(), Kind::AndroidAuto);
        assert_eq!(session.link(), Link::Wifi);
        assert_eq!(session.phase(), Phase::Spawned);
        assert_eq!(session.cipher_name(), "none");
        let port = session.wireless_port().expect("a port");
        assert_ne!(port, 0);
        // Nothing to send a touch to yet; it is dropped, not queued.
        session.command(Cmd::Rotary(1));
        assert!(events(&ports).is_empty());

        let mut phone = connect_phone(port);
        phone.connect();
        assert_eq!(session.cipher_name(), phone.tls.cipher_name());
        phone.send_proto(
            CH_CONTROL,
            MSG_SERVICE_DISCOVERY_REQUEST,
            &proto::ServiceDiscoveryRequest {
                device_name: Some("Pixel".into()),
                phone_info: Some(proto::PhoneInfo {
                    instance_id: Some("abc".into()),
                }),
            },
        );
        assert_eq!(phone.read().unwrap().1, MSG_SERVICE_DISCOVERY_RESPONSE);
        let mut got = Vec::new();
        wait_for(&ports, &mut got, Phase::Identified);
        assert_eq!(session.phone(), Some("Pixel"));
        assert_eq!(session.identity().as_deref(), Some("abc"));
        assert_eq!(session.link(), Link::Wifi);

        // One phone at a time: a second connection is closed at once.
        let mut second = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut b = [0u8; 4];
        assert!(matches!(second.read(&mut b), Ok(0) | Err(_)));
        assert_eq!(
            session.phase(),
            Phase::Identified,
            "the session is left alone"
        );

        // The phone leaves Wi-Fi: its link breaks, the session is lost.
        drop(phone);
        wait_for(&ports, &mut got, Phase::Closed);
        assert!(got.contains(&Event::Fault(Fault::LinkLost)), "{got:?}");
        assert_eq!(session.phase(), Phase::Closed);
        let start = std::time::Instant::now();
        session.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
        // The port is free again for the next session.
        assert!(crate::tcp::Listener::bind(port).is_ok());
    }

    #[test]
    fn a_waiting_wireless_session_ends_on_close_and_needs_an_access_point() {
        let bt = lehu_bt::Manager::start(&Default::default(), |_| {}).unwrap();
        let wake = Arc::new(Wake::new().unwrap());
        let video = Arc::new(Queue::new(8));
        let ports = Ports::new(wake, Arc::clone(&video), video);
        let sunk = Arc::new(Mutex::new(Vec::new()));
        let r = Session::start_wireless_on(
            config(),
            &bt,
            lehu_wifi::Params::default(),
            Box::new(FakeSinks(Arc::clone(&sunk))),
            ports.clone(),
            0,
        );
        let e = r.err().expect("no access point");
        assert!(matches!(e, Error::NoAccessPoint), "{e:?}");
        assert_eq!(e.to_string(), "the access point is not up");

        let (session, ports, _bt) = wireless();
        session.command(Cmd::Close);
        let mut got = Vec::new();
        wait_for(&ports, &mut got, Phase::Closed);
        assert_eq!(got, [Event::Phase(Phase::Closed)]);
        assert_eq!(session.phase(), Phase::Closed);
        session.stop();

        // Stopped while waiting: quick, and the port let go.
        let (session, _ports, _bt) = wireless();
        let port = session.wireless_port().unwrap();
        let start = std::time::Instant::now();
        session.stop();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(crate::tcp::Listener::bind(port).is_ok());
        // And dropped while waiting, the abrupt end.
        let (session, _ports, _bt) = wireless();
        drop(session);
    }
}
